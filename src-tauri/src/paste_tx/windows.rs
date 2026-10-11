//! Windows reliable paste.
//!
//! Publishes the transcript as a *delayed-render* clipboard format
//! (`SetClipboardData(CF_UNICODETEXT, NULL)`) owned by a hidden message-only
//! window. Windows sends the owner `WM_RENDERFORMAT` when a consumer actually
//! requests the data — that message is the read receipt. The previous
//! clipboard contents (snapshotted with full format fidelity) are restored
//! once receipts go quiet (see `paste_tx::evaluate`), guarded by the clipboard
//! owner and sequence number, both rechecked under the clipboard lock, so we
//! never clobber a newer user copy.
//!
//! Threading: clipboard ownership and delayed rendering are per-thread and
//! need a message pump, so the whole transaction lives on a dedicated worker
//! thread. The calling thread only sends the paste chord once the worker
//! signals the transcript is published, then returns; the wait, guarded
//! restore and auto-submit all finish on the worker.
//!
//! Two transaction modes share this machinery:
//! - `Dictation`: the debug-gated reliable dictation paste (best-effort
//!   snapshot, optional auto-submit, legacy fallback on start failure).
//! - `Strict`: the assistant replacement commit (`StrictClipboardTx`): a
//!   lossless snapshot or refusal, no auto-submit, and an explicit settlement
//!   report the caller waits for.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use log::{error, info, warn};
use tauri::Manager;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, GlobalFree, SetLastError, ERROR_SUCCESS, HANDLE, HGLOBAL, HINSTANCE, HWND,
    LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CopyImage, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, KillTimer, MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage,
    RegisterClassW, SetTimer, SetWindowLongPtrW, GDI_IMAGE_TYPE, GWLP_USERDATA, HWND_MESSAGE,
    IMAGE_FLAGS, MSG, MWMO_INPUTAVAILABLE, PM_NOREMOVE, PM_QS_SENDMESSAGE, QS_SENDMESSAGE,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROYCLIPBOARD, WM_RENDERALLFORMATS, WM_RENDERFORMAT,
    WM_TIMER, WNDCLASSW,
};

use super::{
    adopt_render_sequence, evaluate, format_action, judge_read, on_render_all_request,
    on_render_request, preempt, retry_aborted_restore, retry_clipboard_open, send_chord,
    settle_guarded, snapshot_and_publish, strict_settle_action, ClipboardOps, ClipboardSettlement,
    FormatAction, FormatRead, LossyFormat, LossyReason, PublishError, SettleAction, SnapshotPolicy,
    StrictSettlement, TxState, WaitDecision, CF_BITMAP, CF_UNICODETEXT, RESTORE_TIMEOUT,
};
use crate::clipboard::send_return_key;
use crate::input::EnigoState;
use crate::settings::{AutoSubmitKey, ClipboardHandling, PasteMethod};

const CLASS_NAME: PCWSTR = w!("HandyPasteTxWindow");
const TIMER_ID: usize = 1;
const TIMER_INTERVAL_MS: u32 = 25;
/// `OpenClipboard` attempts before giving up (another process may hold it).
const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(25);
/// How long a strict caller waits for the worker to publish.
const STRICT_PUBLISH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a preemptor, or an abort, waits for a settlement in progress.
const SETTLE_WAIT: Duration = Duration::from_secs(2);

const IMAGE_BITMAP_TYPE: GDI_IMAGE_TYPE = GDI_IMAGE_TYPE(0);
const LR_CREATEDIBSECTION_FLAG: IMAGE_FLAGS = IMAGE_FLAGS(0x2000);

struct SavedFormat {
    format: u32,
    data: Vec<u8>,
}

/// Clipboard snapshot. Owns a copied HBITMAP until it is handed to the
/// clipboard; dropping it otherwise deletes the bitmap.
#[derive(Default)]
pub(super) struct WinSnapshot {
    formats: Vec<SavedFormat>,
    /// Copied HBITMAP as a raw value (HBITMAP is not `Send`).
    bitmap: Option<usize>,
}

impl Drop for WinSnapshot {
    fn drop(&mut self) {
        if let Some(raw) = self.bitmap.take() {
            // SAFETY: the bitmap was created by CopyImage for this snapshot
            // and was never handed to the clipboard (that path takes it).
            unsafe {
                let _ = DeleteObject(HGDIOBJ(raw as *mut _));
            }
        }
    }
}

/// Sends the dictation auto-submit key. Built by the dictation entry point so
/// the shared settle code never references tauri state.
type AutoSubmitAction = Box<dyn Fn() + Send + Sync>;

enum TxMode {
    Dictation {
        /// `Some` when auto-submit is enabled.
        auto_submit: Option<AutoSubmitAction>,
        /// ClipboardHandling::CopyToClipboard — settle by leaving the
        /// transcript on the clipboard as plain text instead of restoring.
        preserve_transcript: bool,
    },
    Strict {
        after_success: ClipboardHandling,
    },
}

pub(super) struct WinTxShared {
    state: Mutex<TxState>,
    text: String,
    policy: SnapshotPolicy,
    snapshot: Mutex<Option<WinSnapshot>>,
    sequence: Mutex<u32>,
    /// The owner window (raw HWND), 0 until created.
    hwnd: AtomicUsize,
    /// The publishing caller gave up waiting: do not publish.
    abandoned: AtomicBool,
    /// A settlement is in progress or completed. Cleared for abort retries.
    settled: AtomicBool,
    /// Set once settlement completed.
    outcome: Mutex<Option<StrictSettlement>>,
    outcome_ready: Condvar,
    mode: TxMode,
}

impl WinTxShared {
    fn new(text: &str, policy: SnapshotPolicy, mode: TxMode) -> Self {
        Self {
            state: Mutex::new(TxState::new()),
            text: text.to_string(),
            policy,
            snapshot: Mutex::new(None),
            sequence: Mutex::new(0),
            hwnd: AtomicUsize::new(0),
            abandoned: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            outcome: Mutex::new(None),
            outcome_ready: Condvar::new(),
            mode,
        }
    }

    fn hwnd(&self) -> HWND {
        HWND(self.hwnd.load(Ordering::SeqCst) as *mut _)
    }

    fn sequence(&self) -> u32 {
        *self.sequence.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Waits until settlement completed, bounded.
    fn wait_outcome(&self, timeout: Duration) -> Option<StrictSettlement> {
        let deadline = Instant::now() + timeout;
        let mut slot = self.outcome.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(outcome) = slot.as_ref() {
                return Some(outcome.clone());
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            slot = match self.outcome_ready.wait_timeout(slot, deadline - now) {
                Ok((guard, _)) => guard,
                Err(p) => p.into_inner().0,
            };
        }
    }
}

/// The transaction currently holding the clipboard, if any. A new
/// transaction settles it before snapshotting (see `flush_pending`).
static PENDING: Mutex<Option<Arc<WinTxShared>>> = Mutex::new(None);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn shared_ptr(hwnd: HWND) -> *const WinTxShared {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WinTxShared
}

/// Sends the auto-submit Enter. Uses `try_lock` because the paste caller may
/// currently hold the enigo lock while waiting for this worker.
fn send_auto_submit(shared: &WinTxShared) {
    let TxMode::Dictation {
        auto_submit: Some(action),
        ..
    } = &shared.mode
    else {
        return;
    };
    {
        let mut st = match shared.state.lock() {
            Ok(st) => st,
            Err(_) => return,
        };
        if st.auto_submit_sent {
            return;
        }
        st.auto_submit_sent = true;
    }
    action();
}

/// Writes `text` as CF_UNICODETEXT into the clipboard, which must already be
/// open: the system opens it on our behalf for WM_RENDERFORMAT; every other
/// caller has to wrap this in OpenClipboard/CloseClipboard itself.
unsafe fn render_text(text: &str) -> Result<(), String> {
    #[cfg(test)]
    let before = GetClipboardSequenceNumber();
    let wide_text: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let hg = GlobalAlloc(GMEM_MOVEABLE, wide_text.len() * 2)
        .map_err(|e| format!("GlobalAlloc failed: {e}"))?;
    let ptr = GlobalLock(hg) as *mut u16;
    if ptr.is_null() {
        let _ = GlobalFree(Some(hg));
        return Err("GlobalLock failed".into());
    }
    std::ptr::copy_nonoverlapping(wide_text.as_ptr(), ptr, wide_text.len());
    let _ = GlobalUnlock(hg);
    if let Err(e) = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hg.0))) {
        let _ = GlobalFree(Some(hg));
        return Err(format!("SetClipboardData failed: {e}"));
    }
    #[cfg(test)]
    diag::note_locked_change(before, GetClipboardSequenceNumber());
    Ok(())
}

unsafe extern "system" fn paste_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let shared = shared_ptr(hwnd);
    match msg {
        WM_RENDERFORMAT => {
            if !shared.is_null() {
                let shared = &*shared;
                let strict = matches!(shared.mode, TxMode::Strict { .. });
                // See `on_render_request` for the strict pre-injection rule.
                let render = match shared.state.lock() {
                    Ok(mut st) => {
                        // Only the ignored diagnostic can disable D1. Production
                        // always declines eager reads before injection.
                        #[cfg(test)]
                        let strict = strict && diag::DECLINE.load(Ordering::SeqCst);
                        #[cfg(test)]
                        let injected = st.injected_at;
                        let r = on_render_request(&mut st, strict, Instant::now());
                        #[cfg(test)]
                        diag::record(diag::Event::Render {
                            at: Instant::now(),
                            after_injection: injected.is_some(),
                            rendered: r,
                            reader_pid: diag::open_clipboard_pid(),
                        });
                        r
                    }
                    Err(_) => !strict,
                };
                if render && wparam.0 as u32 == CF_UNICODETEXT {
                    // Whether rendering bumps the sequence number is not
                    // documented; adopt a bump only if the value before it
                    // was ours, so the settle guard keeps working either way.
                    let before = GetClipboardSequenceNumber();
                    if let Err(e) = render_text(&shared.text) {
                        warn!("[reliable-paste] rendering the promise failed: {e}");
                    }
                    let after = GetClipboardSequenceNumber();
                    if let Ok(mut seq) = shared.sequence.lock() {
                        *seq = adopt_render_sequence(*seq, before, after);
                    }
                }
            }
            LRESULT(0)
        }
        WM_RENDERALLFORMATS => {
            // Sent when the window is destroyed while an unrendered promise is
            // still on the clipboard — not a consumer read, so no receipt.
            // Unlike WM_RENDERFORMAT the system does not open the clipboard on
            // our behalf here: open it and confirm we still own it first.
            if !shared.is_null() {
                let shared = &*shared;
                let strict = matches!(shared.mode, TxMode::Strict { .. });
                let allowed = shared
                    .state
                    .lock()
                    .map(|st| on_render_all_request(&st, strict))
                    .unwrap_or(!strict);
                if !allowed {
                    return LRESULT(0);
                }
                if OpenClipboard(Some(hwnd)).is_ok() {
                    if GetClipboardOwner()
                        .map(|owner| owner == hwnd)
                        .unwrap_or(false)
                    {
                        let _ = render_text(&shared.text);
                    }
                    let _ = CloseClipboard();
                }
            }
            LRESULT(0)
        }
        WM_DESTROYCLIPBOARD => {
            if !shared.is_null() {
                if let Ok(mut st) = (&*shared).state.lock() {
                    st.ownership_lost = true;
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if !shared.is_null() {
                on_timer(&*shared);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn ensure_window_class(hinstance: HINSTANCE) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(paste_wnd_proc),
            hInstance: hinstance,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        unsafe {
            RegisterClassW(&wc);
        }
    });
}

/// `ClipboardOps` over the Win32 clipboard for one transaction.
struct WinClipboard<'a> {
    shared: &'a WinTxShared,
    /// Window passed to `OpenClipboard`. Publishing must use the owner window
    /// (EmptyClipboard then makes it the owner of the delayed promise);
    /// settlement opens with `None`, as the restore path always has.
    open_as: Option<HWND>,
    is_open: bool,
    #[cfg(test)]
    opened_sequence: u32,
}

impl<'a> WinClipboard<'a> {
    fn new(shared: &'a WinTxShared, open_as: Option<HWND>) -> Self {
        Self {
            shared,
            open_as,
            is_open: false,
            #[cfg(test)]
            opened_sequence: 0,
        }
    }
}

impl Drop for WinClipboard<'_> {
    fn drop(&mut self) {
        self.close();
    }
}

/// A format whose explicit write failed is still restored when the system
/// synthesizes it from a format already written (e.g. CF_DIBV5 and CF_BITMAP
/// from CF_DIB, CF_TEXT from CF_UNICODETEXT); observed live for CF_DIB.
unsafe fn synthesized(format: u32) -> bool {
    IsClipboardFormatAvailable(format).is_ok()
}

/// Dispatch only nonqueued sent messages while waiting for a clipboard reader.
/// A GetClipboardData sender can hold the clipboard until our WM_RENDERFORMAT
/// returns. Do not dispatch timers here: settlement holds the snapshot lock.
fn wait_for_clipboard_retry(delay: Duration) {
    #[cfg(test)]
    diag::run_on_clipboard_retry();
    let deadline = Instant::now() + delay;
    loop {
        // SAFETY: PeekMessage dispatches sent messages without removing queued
        // messages; WM_RENDERFORMAT takes no snapshot lock and declines D1 reads.
        unsafe {
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE | PM_QS_SENDMESSAGE);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let _ = MsgWaitForMultipleObjectsEx(
                None,
                remaining.as_millis().max(1) as u32,
                QS_SENDMESSAGE,
                MWMO_INPUTAVAILABLE,
            );
        }
    }
}

/// Reads one global-memory format. Returns the read classification and the
/// bytes when they could be copied.
unsafe fn read_memory_format(format: u32) -> (FormatRead, Option<Vec<u8>>) {
    let Ok(handle) = GetClipboardData(format) else {
        return (FormatRead::Unreadable, None);
    };
    let hg = HGLOBAL(handle.0);
    let size = GlobalSize(hg);
    if size == 0 || size > super::MAX_FORMAT_BYTES {
        return (
            FormatRead::Memory {
                size,
                locked: false,
            },
            None,
        );
    }
    let ptr = GlobalLock(hg) as *const u8;
    if ptr.is_null() {
        return (
            FormatRead::Memory {
                size,
                locked: false,
            },
            None,
        );
    }
    let data = std::slice::from_raw_parts(ptr, size).to_vec();
    let _ = GlobalUnlock(hg);
    (FormatRead::Memory { size, locked: true }, Some(data))
}

impl ClipboardOps for WinClipboard<'_> {
    type Snapshot = WinSnapshot;

    fn open(&mut self) -> Result<(), String> {
        retry_clipboard_open(
            OPEN_ATTEMPTS,
            OPEN_RETRY_DELAY,
            || unsafe { OpenClipboard(self.open_as) }.map_err(|e| e.to_string()),
            wait_for_clipboard_retry,
        )?;
        self.is_open = true;
        #[cfg(test)]
        {
            self.opened_sequence = self.sequence();
            diag::note_locked_change(self.opened_sequence, self.opened_sequence);
        }
        Ok(())
    }

    fn close(&mut self) {
        if self.is_open {
            #[cfg(test)]
            diag::note_locked_change(self.opened_sequence, self.sequence());
            self.is_open = false;
            if let Err(e) = unsafe { CloseClipboard() } {
                warn!("[reliable-paste] CloseClipboard failed: {e}");
            }
        }
    }

    fn sequence(&self) -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    fn owned_by_tx(&self) -> bool {
        let ours = self.shared.hwnd();
        !ours.is_invalid()
            && unsafe { GetClipboardOwner() }
                .map(|owner| owner == ours)
                .unwrap_or(false)
    }

    fn snapshot(&mut self, policy: SnapshotPolicy) -> Result<WinSnapshot, LossyFormat> {
        let mut snapshot = WinSnapshot::default();
        let mut format = 0u32;
        loop {
            // EnumClipboardFormats returns 0 both at the end and on failure;
            // only a nonzero thread error distinguishes them.
            unsafe { SetLastError(ERROR_SUCCESS) };
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                if unsafe { GetLastError() } != ERROR_SUCCESS && policy == SnapshotPolicy::Strict {
                    return Err(LossyFormat {
                        format: 0,
                        reason: LossyReason::EnumerationFailed,
                    });
                }
                break;
            }
            match format_action(policy, format) {
                FormatAction::Skip => {}
                FormatAction::Reject(reason) => return Err(LossyFormat { format, reason }),
                FormatAction::CopyBitmap => {
                    // GDI object, not global memory: duplicate the bitmap.
                    let copy = unsafe {
                        GetClipboardData(CF_BITMAP).ok().and_then(|handle| {
                            CopyImage(handle, IMAGE_BITMAP_TYPE, 0, 0, LR_CREATEDIBSECTION_FLAG)
                                .ok()
                        })
                    };
                    let read = FormatRead::Bitmap {
                        copied: copy.is_some(),
                    };
                    match judge_read(policy, read) {
                        Ok(true) => {
                            if let Some(copy) = copy {
                                if let Some(old) = snapshot.bitmap.replace(copy.0 as usize) {
                                    unsafe {
                                        let _ = DeleteObject(HGDIOBJ(old as *mut _));
                                    }
                                }
                            }
                        }
                        Ok(false) => {}
                        Err(reason) => return Err(LossyFormat { format, reason }),
                    }
                }
                FormatAction::ReadMemory => {
                    let (read, data) = unsafe { read_memory_format(format) };
                    match judge_read(policy, read) {
                        Ok(true) => {
                            if let Some(data) = data {
                                snapshot.formats.push(SavedFormat { format, data });
                            }
                        }
                        Ok(false) => {}
                        Err(reason) => return Err(LossyFormat { format, reason }),
                    }
                }
            }
        }
        Ok(snapshot)
    }

    fn publish_promise(&mut self) -> Result<(), String> {
        unsafe { publish_formats() }
    }

    fn write_snapshot(&mut self, snapshot: &mut WinSnapshot) -> Result<(), String> {
        unsafe {
            EmptyClipboard().map_err(|e| format!("EmptyClipboard failed: {e}"))?;
            let mut failed: Vec<u32> = Vec::new();
            for saved in snapshot.formats.iter() {
                if saved.data.is_empty() {
                    continue;
                }
                let Ok(hg) = GlobalAlloc(GMEM_MOVEABLE, saved.data.len()) else {
                    failed.push(saved.format);
                    continue;
                };
                let ptr = GlobalLock(hg) as *mut u8;
                if ptr.is_null() {
                    let _ = GlobalFree(Some(hg));
                    failed.push(saved.format);
                    continue;
                }
                std::ptr::copy_nonoverlapping(saved.data.as_ptr(), ptr, saved.data.len());
                let _ = GlobalUnlock(hg);
                // SetClipboardData takes ownership of the handle on success.
                if SetClipboardData(saved.format, Some(HANDLE(hg.0))).is_err() {
                    let _ = GlobalFree(Some(hg));
                    if !synthesized(saved.format) {
                        failed.push(saved.format);
                    }
                }
            }
            if let Some(raw) = snapshot.bitmap.take() {
                if SetClipboardData(CF_BITMAP, Some(HANDLE(raw as *mut _))).is_err() {
                    let _ = DeleteObject(HGDIOBJ(raw as *mut _));
                    if !synthesized(CF_BITMAP) {
                        failed.push(CF_BITMAP);
                    }
                }
            }
            if failed.is_empty() {
                Ok(())
            } else {
                Err(format!("could not restore clipboard formats {failed:?}"))
            }
        }
    }

    fn write_text(&mut self, text: &str) -> Result<(), String> {
        unsafe {
            EmptyClipboard().map_err(|e| format!("EmptyClipboard failed: {e}"))?;
            render_text(text)
        }
    }

    fn abandoned(&self) -> bool {
        self.shared.abandoned.load(Ordering::SeqCst)
    }
}

/// Publishes the transcript as a delayed-render promise plus clipboard
/// history / cloud / monitoring opt-out markers (the same formats Chrome uses
/// for Incognito copies). The clipboard must be open with the owner window.
unsafe fn publish_formats() -> Result<(), String> {
    EmptyClipboard().map_err(|e| format!("EmptyClipboard failed: {e}"))?;

    for (name, value) in [
        ("ExcludeClipboardContentFromMonitorProcessing", 1u32),
        ("CanIncludeInClipboardHistory", 0u32),
        ("CanUploadToCloudClipboard", 0u32),
    ] {
        let name_wide = wide(name);
        let format = RegisterClipboardFormatW(PCWSTR(name_wide.as_ptr()));
        if format == 0 {
            continue;
        }
        if let Ok(hg) = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of::<u32>()) {
            let ptr = GlobalLock(hg) as *mut u32;
            if !ptr.is_null() {
                *ptr = value;
                let _ = GlobalUnlock(hg);
                if SetClipboardData(format, Some(HANDLE(hg.0))).is_err() {
                    let _ = GlobalFree(Some(hg));
                }
            } else {
                let _ = GlobalFree(Some(hg));
            }
        }
    }

    // NULL handle = delayed rendering: we are only asked for the data (via
    // WM_RENDERFORMAT) when a consumer actually reads it. SetClipboardData
    // returns the handle it was given, so for delayed rendering success is
    // also NULL and the windows crate reports it as an Err carrying
    // GetLastError(). Only a nonzero thread error is a real failure, and the
    // thread error must be cleared first so a stale value from an earlier
    // call can't masquerade as one.
    SetLastError(ERROR_SUCCESS);
    if let Err(e) = SetClipboardData(CF_UNICODETEXT, None) {
        if e.code().is_err() {
            return Err(format!("SetClipboardData failed: {e}"));
        }
    }
    Ok(())
}

/// Settles a transaction exactly once: optional auto-submit (dictation), then
/// the lock-guarded clipboard settlement, then the outcome report.
/// `auto_submit_now` overrides the receipt-based auto-submit decision (used by
/// a preemptor); `None` means "auto-submit iff the target read the text".
fn finish_tx(shared: &WinTxShared, auto_submit_now: Option<bool>) {
    if shared.settled.swap(true, Ordering::SeqCst) {
        return;
    }
    let (receipt, ownership_lost, injected, injection_failed, aborted, preempted) = {
        let st = shared.state.lock().unwrap_or_else(|p| p.into_inner());
        (
            st.any_receipt_after_injection(),
            st.ownership_lost,
            st.injected_at.is_some(),
            st.injection_failed,
            st.aborted,
            st.preempted,
        )
    };
    if ownership_lost {
        info!("[reliable-paste] settling: clipboard ownership lost");
    } else if aborted {
        info!("[reliable-paste] settling: transaction aborted before delivery");
    } else if preempted {
        info!("[reliable-paste] settling: preempted by a newer paste");
    } else if receipt {
        info!("[reliable-paste] settling: reads went quiet");
    } else if injection_failed {
        info!("[reliable-paste] settling: chord injection failed, restoring quickly");
    } else {
        info!("[reliable-paste] settling: no read within timeout, restoring anyway");
    }

    let action = match &shared.mode {
        TxMode::Dictation {
            auto_submit,
            preserve_transcript,
            ..
        } => {
            // Auto-submit only once the target demonstrably read the
            // transcript; pressing Enter after an unconfirmed paste could
            // submit stale content.
            if auto_submit.is_some() && auto_submit_now.unwrap_or(receipt) {
                send_auto_submit(shared);
            }
            if *preserve_transcript {
                SettleAction::LeaveText
            } else {
                SettleAction::Restore
            }
        }
        TxMode::Strict { after_success } => {
            strict_settle_action(injected, injection_failed, aborted, *after_success)
        }
    };

    let expected = shared.sequence();
    let clipboard = {
        let mut clip = WinClipboard::new(shared, None);
        let mut snapshot = shared.snapshot.lock().unwrap_or_else(|p| p.into_inner());
        settle_guarded(
            &mut clip,
            snapshot.as_mut(),
            expected,
            ownership_lost,
            action,
            &shared.text,
        )
    };
    match &clipboard {
        ClipboardSettlement::Restored => info!("[reliable-paste] restored previous clipboard"),
        ClipboardSettlement::LeftReplacement => {
            info!("[reliable-paste] left transcript on clipboard as plain text")
        }
        ClipboardSettlement::ExternallyChanged => {
            info!("[reliable-paste] clipboard changed externally; leaving it untouched")
        }
        ClipboardSettlement::RestoreFailed(e) => {
            warn!("[reliable-paste] clipboard settlement failed: {e}")
        }
        other => info!("[reliable-paste] clipboard settlement: {other:?}"),
    }

    if retry_aborted_restore(
        matches!(shared.mode, TxMode::Strict { .. }),
        aborted || !injected || injection_failed,
        &clipboard,
    ) {
        shared.settled.store(false, Ordering::SeqCst);
        // No outcome yet: on_timer retains the window and retries. A later
        // ownership/sequence change will finish as ExternallyChanged.
        return;
    }
    let report = StrictSettlement {
        receipt_after_injection: receipt,
        ownership_lost,
        preempted,
        clipboard,
    };
    let mut slot = shared.outcome.lock().unwrap_or_else(|p| p.into_inner());
    *slot = Some(report);
    shared.outcome_ready.notify_all();
}

/// If a previous transaction is still holding the clipboard, settle it now so
/// the snapshot below captures the user's original clipboard content. A
/// strict caller passes `allow_auto_submit = false`, so a pending dictation
/// Enter can never land in the assistant's target.
fn flush_pending(allow_auto_submit: bool) {
    let previous = match PENDING.lock() {
        Ok(mut slot) => slot.take(),
        Err(_) => None,
    };
    let Some(previous) = previous else {
        return;
    };
    let auto_submit_now = {
        let mut st = previous.state.lock().unwrap_or_else(|p| p.into_inner());
        preempt(&mut st, allow_auto_submit)
    };
    finish_tx(&previous, Some(auto_submit_now));
    // The old worker may have been mid-settlement; do not snapshot until it
    // finished, or the snapshot could capture a half-restored clipboard.
    if previous.wait_outcome(SETTLE_WAIT).is_none() {
        warn!("[reliable-paste] previous transaction did not settle in time");
    }
}

fn on_timer(shared: &WinTxShared) {
    let now = Instant::now();
    let finish = {
        let mut st = match shared.state.lock() {
            Ok(st) => st,
            Err(_) => return,
        };
        if st.cancelled {
            true
        } else {
            match evaluate(&st, now) {
                WaitDecision::KeepWaiting => false,
                WaitDecision::Finish => {
                    st.cancelled = true;
                    true
                }
            }
        }
    };
    if !finish {
        return;
    }

    finish_tx(shared, None);
    // A preemptor may be settling this transaction on another thread. Keep
    // the owner window alive and pumping until it is done: its ownership
    // check needs the window, and its EmptyClipboard sends this window
    // WM_DESTROYCLIPBOARD, so blocking here would stall it. Check again on
    // the next tick.
    let settled = shared
        .outcome
        .lock()
        .map(|slot| slot.is_some())
        .unwrap_or(true);
    if !settled {
        return;
    }

    if let Ok(mut slot) = PENDING.lock() {
        let is_us = slot
            .as_ref()
            .map(|pending| std::ptr::eq(Arc::as_ptr(pending), shared))
            .unwrap_or(false);
        if is_us {
            *slot = None;
        }
    }

    unsafe {
        PostQuitMessage(0);
    }
}

unsafe fn destroy_window_and_shared(hwnd: HWND) {
    let ptr = shared_ptr(hwnd);
    let _ = DestroyWindow(hwnd);
    if !ptr.is_null() {
        drop(Arc::from_raw(ptr));
    }
}

fn pump_thread(
    shared: Arc<WinTxShared>,
    allow_auto_submit_on_flush: bool,
    ready: mpsc::Sender<Result<(), PublishError>>,
) {
    unsafe {
        // Settle any previous transaction first so the snapshot captures the
        // user's original clipboard, not the previous transcript.
        flush_pending(allow_auto_submit_on_flush);

        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(hmodule) => HINSTANCE(hmodule.0),
            Err(e) => {
                let _ = ready.send(Err(PublishError::Busy(format!(
                    "GetModuleHandle failed: {e}"
                ))));
                return;
            }
        };
        ensure_window_class(hinstance);

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS_NAME,
            w!("HandyPasteTx"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance),
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(e) => {
                let _ = ready.send(Err(PublishError::Busy(format!(
                    "CreateWindowEx failed: {e}"
                ))));
                return;
            }
        };
        shared.hwnd.store(hwnd.0 as usize, Ordering::SeqCst);
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            Arc::into_raw(shared.clone()) as *const _ as isize,
        );

        let published = {
            let mut clip = WinClipboard::new(&shared, Some(hwnd));
            snapshot_and_publish(&mut clip, shared.policy)
        };
        let (snapshot, sequence) = match published {
            Ok(published) => published,
            Err(e) => {
                // Nothing to settle: either nothing changed, or the publish
                // failure path already wrote the snapshot back.
                shared.settled.store(true, Ordering::SeqCst);
                destroy_window_and_shared(hwnd);
                let _ = ready.send(Err(e));
                return;
            }
        };
        *shared.snapshot.lock().unwrap_or_else(|p| p.into_inner()) = Some(snapshot);
        *shared.sequence.lock().unwrap_or_else(|p| p.into_inner()) = sequence;
        shared
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .published_at = Instant::now();
        if let Ok(mut slot) = PENDING.lock() {
            *slot = Some(shared.clone());
        }
        let _ = SetTimer(Some(hwnd), TIMER_ID, TIMER_INTERVAL_MS, None);
        let _ = ready.send(Ok(()));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = DispatchMessageW(&msg);
        }

        let _ = KillTimer(Some(hwnd), TIMER_ID);
        destroy_window_and_shared(hwnd);
    }
}

pub(super) fn run(
    text: &str,
    app_handle: &tauri::AppHandle,
    paste_method: &PasteMethod,
    enigo: &mut enigo::Enigo,
    auto_submit: bool,
    auto_submit_key: AutoSubmitKey,
    clipboard_handling: ClipboardHandling,
) -> Result<(), String> {
    let shared = Arc::new(WinTxShared::new(
        text,
        SnapshotPolicy::BestEffort,
        TxMode::Dictation {
            auto_submit: auto_submit.then(|| {
                let app_handle = app_handle.clone();
                Box::new(move || {
                    if let Some(enigo_state) = app_handle.try_state::<EnigoState>() {
                        match enigo_state.0.try_lock() {
                            Ok(mut enigo) => {
                                let _ = send_return_key(&mut enigo, auto_submit_key);
                            }
                            Err(_) => {
                                warn!("[reliable-paste] skipping auto-submit: input state busy")
                            }
                        }
                    }
                }) as AutoSubmitAction
            }),
            preserve_transcript: clipboard_handling == ClipboardHandling::CopyToClipboard,
        },
    ));

    let (ready_tx, ready_rx) = mpsc::channel();
    let shared_for_pump = shared.clone();
    thread::spawn(move || pump_thread(shared_for_pump, true, ready_tx));

    // Wait until the transcript is actually published (or the worker reports
    // why it could not) before injecting the chord.
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("{e:?}")),
        Err(_) => return Err("reliable paste worker died before publishing".to_string()),
    }
    info!("[reliable-paste] published transcript (delayed render)");

    // Mark injection *before* sending: enigo holds the chord for ~100ms and a
    // fast target may legitimately read while the chord is still held.
    shared.state.lock().unwrap().injected_at = Some(Instant::now());
    match send_chord(enigo, paste_method) {
        Ok(()) => {
            info!("[reliable-paste] paste chord sent ({paste_method:?})");
        }
        Err(e) => {
            // Keep the transaction alive: the worker restores the clipboard
            // after the short failed-injection timeout.
            shared.state.lock().unwrap().injection_failed = true;
            error!("[reliable-paste] failed to send paste chord: {e}");
        }
    }

    Ok(())
}

/// Strict (assistant) clipboard transaction: lossless snapshot or refusal,
/// receipt-sequenced restore, no auto-submit, explicit settlement report.
pub(crate) struct StrictClipboardTx {
    shared: Arc<WinTxShared>,
}

impl StrictClipboardTx {
    /// Snapshots and publishes `text` (verbatim) on a new pump thread.
    /// Waits at most `STRICT_PUBLISH_TIMEOUT`.
    pub(crate) fn publish(
        text: &str,
        after_success: ClipboardHandling,
    ) -> Result<Self, PublishError> {
        let shared = Arc::new(WinTxShared::new(
            text,
            SnapshotPolicy::Strict,
            TxMode::Strict { after_success },
        ));
        let (ready_tx, ready_rx) = mpsc::channel();
        let shared_for_pump = shared.clone();
        thread::Builder::new()
            .name("handy-strict-paste".into())
            .spawn(move || pump_thread(shared_for_pump, false, ready_tx))
            .map_err(|e| PublishError::Busy(format!("could not start paste worker: {e}")))?;
        match ready_rx.recv_timeout(STRICT_PUBLISH_TIMEOUT) {
            Ok(Ok(())) => Ok(Self { shared }),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Stop a late publish; if it already published, the pump sees
                // `aborted` and restores on its own.
                shared.abandoned.store(true, Ordering::SeqCst);
                if let Ok(mut st) = shared.state.lock() {
                    st.aborted = true;
                    st.cancelled = true;
                }
                Err(PublishError::Timeout)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(PublishError::Busy(
                "paste worker exited before publishing".into(),
            )),
        }
    }

    /// The published promise is still the clipboard content. Advisory check
    /// before injection; settlement re-checks under the clipboard lock.
    pub(crate) fn still_owned(&self) -> bool {
        let lost = self
            .shared
            .state
            .lock()
            .map(|st| st.ownership_lost)
            .unwrap_or(true);
        if lost {
            return false;
        }
        let clip = WinClipboard::new(&self.shared, None);
        clip.owned_by_tx() && clip.sequence() == self.shared.sequence()
    }

    /// Call immediately before sending the chord: only reads after this
    /// count as receipts.
    pub(crate) fn mark_injecting(&self) {
        if let Ok(mut st) = self.shared.state.lock() {
            st.injected_at = Some(Instant::now());
        }
        #[cfg(test)]
        diag::record(diag::Event::Injecting {
            at: Instant::now(),
            holder_pid: diag::open_clipboard_pid(),
        });
    }

    /// No process (including this one) has the clipboard open right now.
    /// Checked by acquiring and immediately releasing it, because readers
    /// that open the clipboard without a window are invisible to
    /// `GetOpenClipboardWindow`. Opening and closing does not change the
    /// clipboard content or sequence number.
    pub(crate) fn clipboard_free() -> bool {
        // SAFETY: balanced open/close on this thread; nothing is written.
        let free = unsafe {
            if OpenClipboard(None).is_ok() {
                let _ = CloseClipboard();
                true
            } else {
                false
            }
        };
        #[cfg(test)]
        let holder_pid = if free { 0 } else { diag::open_clipboard_pid() };
        #[cfg(test)]
        diag::record(diag::Event::FreeCheck {
            at: Instant::now(),
            free,
            holder_pid,
        });
        free
    }

    /// No paste keystroke reached the target; restore quickly.
    pub(crate) fn mark_injection_failed(&self) {
        if let Ok(mut st) = self.shared.state.lock() {
            st.injection_failed = true;
        }
    }

    /// Pre-delivery abort: restore if still owned and wait for the result.
    pub(crate) fn abort(&self) -> ClipboardSettlement {
        if let Ok(mut st) = self.shared.state.lock() {
            st.aborted = true;
            st.cancelled = true;
        }
        self.shared
            .wait_outcome(SETTLE_WAIT)
            .map(|s| s.clipboard)
            .unwrap_or(ClipboardSettlement::Unknown)
    }

    /// Waits for the pump's settlement report.
    pub(crate) fn wait_settled(&self, timeout: Duration) -> Option<StrictSettlement> {
        self.shared.wait_outcome(timeout)
    }

    /// Upper bound for `wait_settled` after injection.
    pub(crate) fn settle_timeout() -> Duration {
        RESTORE_TIMEOUT + SETTLE_WAIT
    }
}

/// Session-wide backup for the ignored native probe. It uses the production
/// lossless snapshot/writer and refuses rollback after an external copy.
#[cfg(test)]
pub(crate) struct ProbeClipboardSession {
    shared: WinTxShared,
    snapshot: Option<WinSnapshot>,
}

#[cfg(test)]
impl ProbeClipboardSession {
    pub(crate) fn begin() -> Result<Self, String> {
        let shared = WinTxShared::new(
            "",
            SnapshotPolicy::Strict,
            TxMode::Strict {
                after_success: ClipboardHandling::DontModify,
            },
        );
        let mut clip = WinClipboard::new(&shared, None);
        clip.open()?;
        let snapshot = clip
            .snapshot(SnapshotPolicy::Strict)
            .map_err(|e| format!("cannot preserve the session clipboard: {e:?}"))?;
        diag::arm_session(clip.sequence());
        clip.close();
        drop(clip);
        Ok(Self {
            shared,
            snapshot: Some(snapshot),
        })
    }

    pub(crate) fn restore(&mut self) -> ClipboardSettlement {
        let Some(mut snapshot) = self.snapshot.take() else {
            return ClipboardSettlement::Untouched;
        };
        let mut clip = WinClipboard::new(&self.shared, None);
        let result = match clip.open() {
            Err(e) => ClipboardSettlement::RestoreFailed(e),
            Ok(()) if !diag::session_current(clip.sequence()) => {
                ClipboardSettlement::ExternallyChanged
            }
            Ok(()) => match clip.write_snapshot(&mut snapshot) {
                Ok(()) => ClipboardSettlement::Restored,
                Err(e) => ClipboardSettlement::RestoreFailed(e),
            },
        };
        clip.close();
        diag::disarm_session();
        result
    }
}

#[cfg(test)]
impl Drop for ProbeClipboardSession {
    fn drop(&mut self) {
        let result = self.restore();
        if !matches!(
            result,
            ClipboardSettlement::Untouched | ClipboardSettlement::Restored
        ) {
            eprintln!("probe session clipboard cleanup: {result:?}");
        }
    }
}

/// Test-only toggles and event recorder for
/// the clipboard-contention probe. Compiled to no-ops outside tests.
#[cfg(test)]
pub(crate) mod diag {
    use std::time::Instant;

    #[derive(Debug, Clone)]
    pub(crate) enum Event {
        FreeCheck {
            at: Instant,
            free: bool,
            holder_pid: u32,
        },
        Injecting {
            at: Instant,
            holder_pid: u32,
        },
        Render {
            at: Instant,
            after_injection: bool,
            rendered: bool,
            reader_pid: u32,
        },
    }

    #[cfg(test)]
    pub(crate) static DECLINE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(true);

    #[cfg(test)]
    static EVENTS: std::sync::Mutex<Vec<Event>> = std::sync::Mutex::new(Vec::new());

    #[cfg(test)]
    static SESSION_SEQUENCE: std::sync::Mutex<Option<(u32, bool)>> = std::sync::Mutex::new(None);
    pub(crate) static AFTER_PUBLISH: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> =
        std::sync::Mutex::new(None);
    /// Ignored native probe only: ask the holding reader for the format once
    /// the owner has entered the abort-settlement retry loop.
    #[cfg(test)]
    pub(crate) static ON_CLIPBOARD_RETRY: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> =
        std::sync::Mutex::new(None);
    #[cfg(test)]
    pub(crate) fn run_on_clipboard_retry() {
        let hook = ON_CLIPBOARD_RETRY.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(crate) fn run_after_publish() {
        let hook = AFTER_PUBLISH.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    pub(crate) fn arm_session(sequence: u32) {
        *SESSION_SEQUENCE.lock().unwrap() = Some((sequence, false));
    }

    /// Called only with the clipboard lock held, for the probe's own reads
    /// and writes. An unobserved external copy permanently disables rollback.
    #[cfg(test)]
    pub(crate) fn note_locked_change(before: u32, after: u32) {
        if let Some((expected, lost)) = SESSION_SEQUENCE.lock().unwrap().as_mut() {
            if *expected != before && *expected != after {
                *lost = true;
            }
            *expected = after;
        }
    }

    #[cfg(test)]
    pub(crate) fn session_current(sequence: u32) -> bool {
        SESSION_SEQUENCE
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|(expected, lost)| !lost && *expected == sequence)
    }

    #[cfg(test)]
    pub(crate) fn disarm_session() {
        *SESSION_SEQUENCE.lock().unwrap() = None;
    }

    pub(crate) fn record(event: Event) {
        #[cfg(test)]
        if let Ok(mut events) = EVENTS.lock() {
            events.push(event);
        }
    }

    #[cfg(test)]
    pub(crate) fn take() -> Vec<Event> {
        EVENTS
            .lock()
            .map(|mut e| std::mem::take(&mut *e))
            .unwrap_or_default()
    }

    /// PID owning the window that has the clipboard open (0 if none or the
    /// opener passed no window).
    pub(crate) fn open_clipboard_pid() -> u32 {
        #[cfg(test)]
        {
            use windows::Win32::System::DataExchange::GetOpenClipboardWindow;
            use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
            let mut pid = 0u32;
            // SAFETY: plain Win32 queries.
            unsafe {
                if let Ok(hwnd) = GetOpenClipboardWindow() {
                    GetWindowThreadProcessId(hwnd, Some(&mut pid));
                }
            }
            pid
        }
    }
}
