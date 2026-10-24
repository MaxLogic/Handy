//! Windows selection worker: UI Automation capture and validation, WinEvent
//! invalidation hooks, and the strict commit platform.
//!
//! One long-lived thread (`handy-selection`) owns every COM object and hook.
//! It runs a message loop: commands arrive over an mpsc channel followed by a
//! `WM_WAKE` thread message, and out-of-context WinEvent callbacks are
//! delivered by the same loop. COM is initialised MTA (UIA clients should
//! not use STA), so cross-process UIA calls do not pump and nothing re-enters
//! the worker during a UIA call. UIA connection and transaction timeouts bound
//! how long a hung provider can block it.

use std::cell::RefCell;
use std::sync::atomic::AtomicU8;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HWND, LPARAM, VARIANT_TRUE, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, GetProcessTimes, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_ARRAY, VT_BOOL, VT_I4};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationCacheRequest, IUIAutomationElement,
    IUIAutomationTextPattern, IUIAutomationTextRange, SetWinEventHook,
    TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start, TextUnit_Character,
    UIA_ClassNamePropertyId, UIA_ControlTypePropertyId, UIA_FrameworkIdPropertyId,
    UIA_IsEnabledPropertyId, UIA_IsPasswordPropertyId, UIA_IsReadOnlyAttributeId,
    UIA_IsTextPatternAvailablePropertyId, UIA_IsValuePatternAvailablePropertyId,
    UIA_NativeWindowHandlePropertyId, UIA_ProcessIdPropertyId, UIA_RuntimeIdPropertyId,
    UIA_TextPatternId, UIA_ValueIsReadOnlyPropertyId, UnhookWinEvent, HWINEVENTHOOK,
    UIA_PROPERTY_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetForegroundWindow, GetGUIThreadInfo, GetMessageW, GetWindowLongPtrW,
    GetWindowThreadProcessId, PeekMessageW, PostThreadMessageW, RealGetWindowClassW, ES_READONLY,
    GUITHREADINFO, GWL_STYLE, MSG, PM_NOREMOVE, PM_REMOVE, WINEVENT_OUTOFCONTEXT, WM_APP, WM_USER,
};

use super::commit::{BeginClipboardError, CommitPlatform};
use super::evidence::{
    self, classify_capture, compare_evidence, CaptureFacts, LiveTarget, OriginalSelection,
    TargetIdentity, WIN32_EDIT_CLASSES,
};
use super::native_scintilla::{self, Target as ScintillaTarget};
use super::native_scintilla_evidence::Error as NativeError;
use super::{
    invalidate, invalidation_of, CaptureError, CaptureReply, Captured, CapturedText,
    ClipboardHandling, ClipboardSettlement, FailReason, InvalidationReason, RefuseReason, Releaser,
    SessionId, StaleReason, TargetId, TargetToken, MAX_SELECTION_SCALARS,
};
use crate::paste_tx::{
    chord_release_keys, classify_chord, ChordResult, PublishError, StrictClipboardTx,
    StrictSettlement,
};

const WM_WAKE: u32 = WM_APP + 1;
/// UIA connection/transaction timeout, in milliseconds.
const UIA_TIMEOUT_MS: u32 = 1000;
const WORKER_START_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// Extra time to receive a worker reply after its own deadline.
const REPLY_GRACE: Duration = Duration::from_millis(50);
/// HRESULT of a UIA call that hit the transaction timeout (UIA_E_TIMEOUT).
const UIA_E_TIMEOUT: i32 = 0x80131505_u32 as i32;

fn release_partial_chord(keys: &[u16]) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    };
    if keys.is_empty() {
        return;
    }
    let inputs: Vec<INPUT> = keys
        .iter()
        .filter(|key| **key != 0x11)
        .map(|key| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(*key),
                    dwFlags: KEYEVENTF_KEYUP,
                    ..Default::default()
                },
            },
        })
        .collect();
    // SAFETY: the slice contains valid key-up INPUT structures.
    if !inputs.is_empty() {
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent != inputs.len() as u32 {
            warn!("[selection] could not release V after partial paste chord");
        }
    }
    if keys.contains(&0x11) {
        crate::input::release_ctrl_best_effort();
    }
}

enum Cmd {
    Capture {
        session: SessionId,
        reply: CaptureReply,
    },
    Validate {
        id: TargetId,
        deadline: Instant,
        reply: Sender<Result<(), StaleReason>>,
    },
    PrepareInjection {
        id: TargetId,
        deadline: Instant,
        reply: Sender<Result<(), StaleReason>>,
    },
    Verify {
        id: TargetId,
        text: String,
        deadline: Instant,
        reply: Sender<bool>,
    },
    Release(TargetId),
    Shutdown(Sender<()>),
}

struct WorkerHandle {
    tx: Sender<Cmd>,
    tid: u32,
}

impl WorkerHandle {
    /// Queues a command and wakes the worker. Never blocks.
    fn post(&self, cmd: Cmd) -> Result<(), Cmd> {
        self.tx.send(cmd).map_err(|e| e.0)?;
        // SAFETY: posting a message to a thread id has no memory effects.
        if let Err(e) = unsafe { PostThreadMessageW(self.tid, WM_WAKE, WPARAM(0), LPARAM(0)) } {
            // Queue full: the command is still in the channel and is picked
            // up on the next wake.
            warn!("[selection] could not wake the selection worker: {e}");
        }
        Ok(())
    }
}

static WORKER: OnceLock<Result<WorkerHandle, String>> = OnceLock::new();

fn worker() -> Result<&'static WorkerHandle, String> {
    WORKER
        .get_or_init(spawn_worker)
        .as_ref()
        .map_err(Clone::clone)
}

/// Starts the worker and waits only until its message queue exists; COM and
/// UIA initialisation happen afterwards on the worker.
fn spawn_worker() -> Result<WorkerHandle, String> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("handy-selection".into())
        .spawn(move || worker_main(rx, ready_tx))
        .map_err(|e| format!("could not start selection worker: {e}"))?;
    let tid = ready_rx
        .recv_timeout(WORKER_START_TIMEOUT)
        .map_err(|_| "selection worker did not start".to_string())?;
    Ok(WorkerHandle { tx, tid })
}

/// Sends a request and waits for its reply until `deadline` (plus grace).
fn request<T>(make: impl FnOnce(Sender<T>) -> Cmd, deadline: Instant) -> Option<T> {
    let w = worker().ok()?;
    let (tx, rx) = mpsc::channel();
    w.post(make(tx)).ok()?;
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()) + REPLY_GRACE)
        .ok()
}

fn releaser() -> Releaser {
    Arc::new(|id| {
        if let Ok(w) = worker() {
            let _ = w.post(Cmd::Release(id));
        }
    })
}

pub(super) fn submit_capture(session: SessionId, reply: CaptureReply) -> Result<(), CaptureError> {
    let w = worker().map_err(CaptureError::WorkerUnavailable)?;
    // On failure the command, and with it the reply's admission guard, drop.
    w.post(Cmd::Capture { session, reply })
        .map_err(|_| CaptureError::WorkerUnavailable("selection worker stopped".into()))
}

/// Live probe only: non-boundary validation of a held token.
#[cfg(test)]
pub(super) fn probe_validate(id: TargetId) -> Result<(), StaleReason> {
    let deadline = Instant::now() + super::VALIDATE_TIMEOUT;
    request(
        |reply| Cmd::Validate {
            id,
            deadline,
            reply,
        },
        deadline,
    )
    .unwrap_or(Err(StaleReason::Unresponsive))
}

pub(super) fn shutdown() {
    let Some(Ok(w)) = WORKER.get() else {
        return;
    };
    let (tx, rx) = mpsc::channel();
    if w.post(Cmd::Shutdown(tx)).is_err() {
        return;
    }
    if rx.recv_timeout(SHUTDOWN_TIMEOUT).is_err() {
        warn!("[selection] worker did not stop within {SHUTDOWN_TIMEOUT:?}; continuing exit");
    }
}

// --- small COM helpers -----------------------------------------------------

/// VARIANT that is cleared (releasing BSTRs, arrays, interfaces) on drop.
struct OwnedVariant(VARIANT);

impl Drop for OwnedVariant {
    fn drop(&mut self) {
        // SAFETY: the VARIANT came from a UIA getter and is owned here.
        unsafe {
            let _ = VariantClear(&mut self.0);
        }
    }
}

impl OwnedVariant {
    fn vt(&self) -> u16 {
        // SAFETY: `vt` is valid for every VARIANT layout.
        unsafe { self.0.Anonymous.Anonymous.vt.0 }
    }

    fn as_bool(&self) -> Option<bool> {
        // SAFETY: boolVal is the active member when vt == VT_BOOL.
        (self.vt() == VT_BOOL.0)
            .then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.boolVal.0 != 0 })
    }

    fn as_i32_vec(&self) -> Option<Vec<i32>> {
        if self.vt() != (VT_ARRAY.0 | VT_I4.0) {
            return None;
        }
        // SAFETY: parray is the active member for VT_ARRAY.
        let psa = unsafe { self.0.Anonymous.Anonymous.Anonymous.parray };
        safearray_i32(psa)
    }
}

fn safearray_i32(psa: *mut SAFEARRAY) -> Option<Vec<i32>> {
    if psa.is_null() {
        return None;
    }
    // SAFETY: psa is a valid one-dimensional VT_I4 SAFEARRAY owned by the
    // caller's VARIANT; data access is balanced by SafeArrayUnaccessData.
    unsafe {
        let lower = SafeArrayGetLBound(psa, 1).ok()?;
        let upper = SafeArrayGetUBound(psa, 1).ok()?;
        let len = usize::try_from(upper - lower + 1).unwrap_or(0);
        let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
        SafeArrayAccessData(psa, &mut data).ok()?;
        let values = if data.is_null() || len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data as *const i32, len).to_vec()
        };
        let _ = SafeArrayUnaccessData(psa);
        Some(values)
    }
}

fn hresult_text(e: &windows::core::Error) -> String {
    format!("{:#010x} {}", e.code().0, e.message())
}

fn capture_provider_error(e: windows::core::Error) -> CaptureError {
    if e.code().0 == UIA_E_TIMEOUT {
        CaptureError::Timeout
    } else {
        CaptureError::Provider(hresult_text(&e))
    }
}

fn stale_provider_error(e: windows::core::Error) -> StaleReason {
    if e.code().0 == UIA_E_TIMEOUT {
        StaleReason::Unresponsive
    } else {
        StaleReason::Provider(hresult_text(&e))
    }
}

fn cached_variant(
    el: &IUIAutomationElement,
    property: UIA_PROPERTY_ID,
) -> windows::core::Result<OwnedVariant> {
    // SAFETY: plain COM call on a live element; the result is owned.
    unsafe { el.GetCachedPropertyValue(property) }.map(OwnedVariant)
}

fn process_created(pid: u32) -> Option<u64> {
    // SAFETY: the handle is closed on every path; out params are locals.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let times = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
        let _ = CloseHandle(handle);
        times.ok()?;
        Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}

fn gui_focus_hwnd(thread_id: u32) -> usize {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: info is a properly sized local.
    match unsafe { GetGUIThreadInfo(thread_id, &mut info) } {
        Ok(()) => info.hwndFocus.0 as usize,
        Err(_) => 0,
    }
}

fn real_class(hwnd: HWND) -> Option<String> {
    let mut buf = [0u16; 256];
    // SAFETY: buf is a writable local buffer.
    let len = unsafe { RealGetWindowClassW(hwnd, &mut buf) } as usize;
    (len > 0).then(|| String::from_utf16_lossy(&buf[..len.min(buf.len())]))
}

fn window_pid_tid(hwnd: HWND) -> (u32, u32) {
    let mut pid = 0u32;
    // SAFETY: pid is a local out param.
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid, tid)
}

// --- WinEvent hooks --------------------------------------------------------

thread_local! {
    /// Invalidation flag of the target whose hooks are installed.
    static HOOK_FLAG: RefCell<Option<Arc<AtomicU8>>> = const { RefCell::new(None) };
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // NAMECHANGE is hooked only for native Notepad++: status updates carry
    // selection/content dirty flags, including anchor-only change-and-return.
    let reason = if event == 0x800C {
        Some(InvalidationReason::TextSelection)
    } else {
        evidence::classify_event(event, id_object)
    };
    if let Some(reason) = reason {
        HOOK_FLAG.with(|slot| {
            if let Ok(slot) = slot.try_borrow() {
                if let Some(flag) = slot.as_ref() {
                    invalidate(flag, reason);
                }
            }
        });
    }
}

fn unhook_all(hooks: &mut Vec<HWINEVENTHOOK>) {
    for hook in hooks.drain(..) {
        // SAFETY: each hook was installed by this thread and not yet removed.
        if !unsafe { UnhookWinEvent(hook) }.as_bool() {
            warn!("[selection] UnhookWinEvent failed");
        }
    }
    HOOK_FLAG.with(|slot| *slot.borrow_mut() = None);
}

/// Installs the four invalidation hooks for `pid` (foreground is global).
fn install_hooks(
    pid: u32,
    flag: &Arc<AtomicU8>,
    native: bool,
) -> Result<Vec<HWINEVENTHOOK>, CaptureError> {
    HOOK_FLAG.with(|slot| *slot.borrow_mut() = Some(flag.clone()));
    let mut specs = vec![
        (evidence::EVENT_SYSTEM_FOREGROUND, 0u32),
        (evidence::EVENT_OBJECT_FOCUS, pid),
        (evidence::EVENT_OBJECT_LOCATIONCHANGE, pid),
        (evidence::EVENT_OBJECT_TEXTSELECTIONCHANGED, pid),
    ];
    if native {
        specs.push((0x800C, pid));
    }
    let mut hooks = Vec::with_capacity(specs.len());
    for (event, process) in specs {
        // SAFETY: out-of-context hook with a 'static callback; removed by
        // unhook_all on this thread.
        let hook = unsafe {
            SetWinEventHook(
                event,
                event,
                None,
                Some(win_event_proc),
                process,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if hook.0.is_null() {
            unhook_all(&mut hooks);
            return Err(CaptureError::Provider(format!(
                "SetWinEventHook({event:#x}) failed"
            )));
        }
        hooks.push(hook);
    }
    Ok(hooks)
}

/// Delivers queued messages, including pending WinEvent callbacks. Wake
/// messages are dropped: their commands stay in the channel and the outer
/// loop drains it.
fn drain_messages() {
    let mut msg = MSG::default();
    // SAFETY: standard message retrieval on the worker thread.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message != WM_WAKE {
                DispatchMessageW(&msg);
            }
        }
    }
}

// --- worker state ------------------------------------------------------------

struct Uia {
    automation: IUIAutomation,
    capture_cache: IUIAutomationCacheRequest,
    focus_cache: IUIAutomationCacheRequest,
}

fn create_uia() -> windows::core::Result<Uia> {
    // SAFETY: COM is initialised (MTA) on this thread.
    unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
        match automation.cast::<IUIAutomation2>() {
            Ok(a2) => {
                if let Err(e) = a2.SetConnectionTimeout(UIA_TIMEOUT_MS) {
                    warn!("[selection] SetConnectionTimeout failed: {e}");
                }
                if let Err(e) = a2.SetTransactionTimeout(UIA_TIMEOUT_MS) {
                    warn!("[selection] SetTransactionTimeout failed: {e}");
                }
            }
            Err(e) => warn!("[selection] IUIAutomation2 unavailable, no UIA timeouts: {e}"),
        }
        let capture_cache = automation.CreateCacheRequest()?;
        for property in [
            UIA_ProcessIdPropertyId,
            UIA_ControlTypePropertyId,
            UIA_FrameworkIdPropertyId,
            UIA_ClassNamePropertyId,
            UIA_NativeWindowHandlePropertyId,
            UIA_IsPasswordPropertyId,
            UIA_IsEnabledPropertyId,
            UIA_IsTextPatternAvailablePropertyId,
            UIA_IsValuePatternAvailablePropertyId,
            UIA_ValueIsReadOnlyPropertyId,
            UIA_RuntimeIdPropertyId,
        ] {
            capture_cache.AddProperty(property)?;
        }
        let focus_cache = automation.CreateCacheRequest()?;
        focus_cache.AddProperty(UIA_ProcessIdPropertyId)?;
        focus_cache.AddProperty(UIA_RuntimeIdPropertyId)?;
        Ok(Uia {
            automation,
            capture_cache,
            focus_cache,
        })
    }
}

struct Tracked {
    id: TargetId,
    target: TrackedTarget,
    flag: Arc<AtomicU8>,
    hooks: Vec<HWINEVENTHOOK>,
}

enum TrackedTarget {
    Uia {
        identity: TargetIdentity,
        range: IUIAutomationTextRange,
    },
    Scintilla(ScintillaTarget),
}

struct Worker {
    uia: Result<Uia, String>,
    target: Option<Tracked>,
    next_id: u64,
    own_pid: u32,
}

/// Facts and live objects from one capture read.
struct CaptureRead {
    facts: CaptureFacts,
    runtime_id: Vec<i32>,
    range: Option<IUIAutomationTextRange>,
}

fn native_capture_error(error: NativeError) -> CaptureError {
    match error {
        NativeError::Unsupported => CaptureError::UnsupportedTarget,
        NativeError::Disabled => CaptureError::Disabled,
        NativeError::ReadOnly => CaptureError::ReadOnly,
        NativeError::MultipleSelections => CaptureError::MultipleSelections,
        NativeError::TooLarge => CaptureError::SelectionTooLarge,
        NativeError::InvalidText => CaptureError::InvalidText,
        NativeError::Changed => CaptureError::TargetChangedDuringCapture,
        NativeError::Timeout => CaptureError::Timeout,
        NativeError::Identity => CaptureError::TargetIdentityUnavailable,
        NativeError::InvalidPosition | NativeError::ReadFailed => {
            CaptureError::Provider("native selection read failed".into())
        }
    }
}

fn native_stale_error(error: NativeError) -> StaleReason {
    match error {
        NativeError::Timeout => StaleReason::Unresponsive,
        NativeError::Identity => StaleReason::FocusChanged,
        NativeError::Changed => StaleReason::SelectionChanged,
        _ => StaleReason::Provider("native target no longer verifiable".into()),
    }
}

impl Worker {
    fn capture_native(
        &mut self,
        session: SessionId,
        foreground: HWND,
        deadline: Instant,
    ) -> Result<Captured, CaptureError> {
        let (pid, _) = window_pid_tid(foreground);
        if pid == self.own_pid {
            return Err(CaptureError::HandyWindowFocused);
        }
        let mut native =
            ScintillaTarget::open(foreground, deadline).map_err(native_capture_error)?;
        // Flush pre-capture dirty flags before installing the native name hook.
        // Capture evidence begins only after hooks are armed.
        native.barrier(deadline).map_err(native_capture_error)?;
        drain_messages();
        let flag = Arc::new(AtomicU8::new(0));
        super::track_live_target(&flag);
        let mut hooks = install_hooks(pid, &flag, true)?;
        let read = native.capture(deadline).and_then(|text| {
            native.barrier(deadline)?;
            Ok(text)
        });
        drain_messages();
        let outcome = read.map_err(native_capture_error).and_then(|text| {
            if invalidation_of(&flag).is_some() {
                return Err(CaptureError::TargetChangedDuringCapture);
            }
            if Instant::now() >= deadline {
                return Err(CaptureError::Timeout);
            }
            Ok(text
                .map(CapturedText::Selected)
                .unwrap_or(CapturedText::Empty))
        });
        let text = match outcome {
            Ok(text) => text,
            Err(error) => {
                unhook_all(&mut hooks);
                return Err(error);
            }
        };
        self.next_id += 1;
        let id = TargetId(self.next_id);
        let empty = matches!(text, CapturedText::Empty);
        self.target = Some(Tracked {
            id,
            target: TrackedTarget::Scintilla(native),
            flag: flag.clone(),
            hooks,
        });
        Ok(Captured {
            text,
            token: TargetToken::new(id, session, empty, flag, releaser()),
        })
    }

    fn release_current(&mut self, reason: Option<InvalidationReason>) {
        if let Some(mut tracked) = self.target.take() {
            if let Some(reason) = reason {
                invalidate(&tracked.flag, reason);
            }
            unhook_all(&mut tracked.hooks);
            debug!("[selection] released target {:?}", tracked.id);
        }
    }

    fn capture(&mut self, session: SessionId) -> Result<Captured, CaptureError> {
        self.release_current(Some(InvalidationReason::Superseded));
        let started = Instant::now();
        // Kept fail-closed until the independent notification/barrier proof.
        let native_foreground = unsafe { GetForegroundWindow() };
        if native_scintilla::ENABLED && ScintillaTarget::recognizes(native_foreground) {
            return self.capture_native(
                session,
                native_foreground,
                started + super::CAPTURE_TIMEOUT,
            );
        }
        let uia = self
            .uia
            .as_ref()
            .map_err(|e| CaptureError::WorkerUnavailable(e.clone()))?;

        // SAFETY: plain Win32 query.
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.is_invalid() {
            return Err(CaptureError::NoForegroundWindow);
        }
        let (pid, tid) = window_pid_tid(foreground);
        if tid == 0 || pid == 0 {
            return Err(CaptureError::TargetIdentityUnavailable);
        }
        if pid == self.own_pid {
            return Err(CaptureError::HandyWindowFocused);
        }
        let created = process_created(pid).ok_or(CaptureError::TargetIdentityUnavailable)?;
        let gui_focus = gui_focus_hwnd(tid);

        // Hooks go in before any UIA read so changes during capture count.
        let flag = Arc::new(AtomicU8::new(0));
        super::track_live_target(&flag);
        let mut hooks = install_hooks(pid, &flag, false)?;
        let read = read_capture(uia, pid, self.own_pid);
        drain_messages();
        let outcome = read.and_then(|read| {
            if invalidation_of(&flag).is_some() {
                return Err(CaptureError::TargetChangedDuringCapture);
            }
            let text = classify_capture(&read.facts)?;
            let range = read.range.ok_or(CaptureError::NoCaret)?;
            if read.runtime_id.is_empty() {
                return Err(CaptureError::TargetIdentityUnavailable);
            }
            Ok((text, range, read.runtime_id, read.facts.focused_pid))
        });
        let (text, range, runtime_id, focused_pid) = match outcome {
            Ok(ok) => ok,
            Err(e) => {
                unhook_all(&mut hooks);
                return Err(e);
            }
        };

        self.next_id += 1;
        let id = TargetId(self.next_id);
        let original = match &text {
            CapturedText::Selected(s) => OriginalSelection::Selected(s.encode_utf16().collect()),
            CapturedText::Empty => OriginalSelection::Empty,
        };
        let empty = matches!(text, CapturedText::Empty);
        self.target = Some(Tracked {
            id,
            target: TrackedTarget::Uia {
                identity: TargetIdentity {
                    foreground_hwnd: foreground.0 as usize,
                    pid,
                    thread_id: tid,
                    process_created: created,
                    gui_focus_hwnd: gui_focus,
                    runtime_id,
                    focused_pid,
                    original,
                },
                range,
            },
            flag: flag.clone(),
            hooks,
        });
        info!(
            "[selection] captured {:?} in {} ms",
            text,
            started.elapsed().as_millis()
        );
        Ok(Captured {
            text,
            token: TargetToken::new(id, session, empty, flag, releaser()),
        })
    }

    fn validate(&mut self, id: TargetId, deadline: Instant) -> Result<(), StaleReason> {
        drain_messages();
        let Some(tracked) = self.target.as_mut().filter(|t| t.id == id) else {
            return Err(StaleReason::Released);
        };
        let result = match &tracked.target {
            TrackedTarget::Uia { identity, range } => {
                let uia = self
                    .uia
                    .as_ref()
                    .map_err(|e| StaleReason::Provider(e.clone()))?;
                let mut live = WinLive::new(uia, range);
                compare_evidence(identity, invalidation_of(&tracked.flag), &mut live)
            }
            TrackedTarget::Scintilla(native) => {
                if let Some(reason) = invalidation_of(&tracked.flag) {
                    Err(StaleReason::Invalidated(reason))
                } else {
                    native
                        .validate(deadline)
                        .and_then(|()| native.barrier(deadline))
                        .map_err(native_stale_error)
                }
            }
        };
        evidence::finish_validation(result, false, &tracked.flag, drain_messages, || {})
    }

    fn verify(&mut self, id: TargetId, text: &str, deadline: Instant) -> bool {
        let Some(tracked) = self.target.as_ref().filter(|t| t.id == id) else {
            return false;
        };
        let TrackedTarget::Uia { identity, range } = &tracked.target else {
            return match &tracked.target {
                TrackedTarget::Scintilla(native) => native.verify(text, deadline),
                _ => false,
            };
        };
        let Ok(uia) = self.uia.as_ref() else {
            return false;
        };
        let check = || -> windows::core::Result<bool> {
            // SAFETY: COM calls on live objects owned by this thread.
            unsafe {
                let el = uia
                    .automation
                    .GetFocusedElementBuildCache(&uia.focus_cache)?;
                let runtime_id = cached_variant(&el, UIA_RuntimeIdPropertyId)?.as_i32_vec();
                if runtime_id.as_ref() != Some(&identity.runtime_id) {
                    return Ok(false);
                }
                let pattern: IUIAutomationTextPattern =
                    el.GetCurrentPatternAs(UIA_TextPatternId)?;
                let selection = pattern.GetSelection()?;
                if selection.Length()? != 1 {
                    return Ok(false);
                }
                let caret = selection.GetElement(0)?;
                if caret.CompareEndpoints(
                    TextPatternRangeEndpoint_Start,
                    &caret,
                    TextPatternRangeEndpoint_End,
                )? != 0
                {
                    return Ok(false);
                }
                let before = caret.Clone()?;
                before.MoveEndpointByUnit(
                    TextPatternRangeEndpoint_Start,
                    TextUnit_Character,
                    -evidence::verify_span(text),
                )?;
                let read = before.GetText(-1)?;
                let empty_target = matches!(identity.original, OriginalSelection::Empty);
                let caret_from_original_start = if empty_target {
                    caret.CompareEndpoints(
                        TextPatternRangeEndpoint_Start,
                        range,
                        TextPatternRangeEndpoint_Start,
                    )?
                } else {
                    0
                };
                Ok(evidence::verify_insertion(
                    empty_target,
                    caret_from_original_start,
                    &String::from_utf16_lossy(&read),
                    text,
                ))
            }
        };
        match check() {
            Ok(matched) => matched,
            Err(e) => {
                debug!("[selection] verification read failed: {}", hresult_text(&e));
                false
            }
        }
    }
}

/// `UIA_IsReadOnlyAttributeId` of a collapsed range at one endpoint of `r`;
/// `None` unless the provider answers with a VT_BOOL.
fn endpoint_read_only(
    r: &IUIAutomationTextRange,
    endpoint: windows::Win32::UI::Accessibility::TextPatternRangeEndpoint,
) -> Option<bool> {
    // SAFETY: COM calls on objects owned by this thread.
    unsafe {
        let point = r.Clone().ok()?;
        point
            .MoveEndpointByRange(
                if endpoint == TextPatternRangeEndpoint_Start {
                    TextPatternRangeEndpoint_End
                } else {
                    TextPatternRangeEndpoint_Start
                },
                r,
                endpoint,
            )
            .ok()?;
        point
            .GetAttributeValue(UIA_IsReadOnlyAttributeId)
            .map(OwnedVariant)
            .ok()
            .and_then(|v| v.as_bool())
    }
}

/// Searches the selection for any run whose `IsReadOnly` text attribute is
/// true. `Some(false)` when the provider finds none, `None` when it cannot
/// answer.
fn interior_read_only(r: &IUIAutomationTextRange) -> Option<bool> {
    let mut value = VARIANT::default();
    // SAFETY: a VT_BOOL VARIANT owns no resources; the COM call runs on an
    // object owned by this thread.
    unsafe {
        (*value.Anonymous.Anonymous).vt = VT_BOOL;
        (*value.Anonymous.Anonymous).Anonymous.boolVal = VARIANT_TRUE;
        match r.FindAttribute(UIA_IsReadOnlyAttributeId, &value, false) {
            Ok(_) => Some(true),
            // A null range (no match) surfaces as an error carrying S_OK.
            Err(e) if e.code().is_ok() => Some(false),
            Err(e) => {
                debug!("[selection] read-only scan failed: {}", hresult_text(&e));
                None
            }
        }
    }
}

/// Reads capture facts. Stops after the element properties when an earlier
/// rule already rejects the target, so unsupported providers are never asked
/// for their text.
fn read_capture(uia: &Uia, foreground_pid: u32, own_pid: u32) -> Result<CaptureRead, CaptureError> {
    // SAFETY: COM calls on objects owned by this thread.
    unsafe {
        let el = uia
            .automation
            .GetFocusedElementBuildCache(&uia.capture_cache)
            .map_err(capture_provider_error)?;
        let mut facts = CaptureFacts {
            own_pid,
            foreground_pid,
            focused_pid: el.CachedProcessId().map_err(capture_provider_error)? as u32,
            framework_id: el
                .CachedFrameworkId()
                .map_err(capture_provider_error)?
                .to_string(),
            control_type: el.CachedControlType().map_err(capture_provider_error)?.0,
            class_name: el
                .CachedClassName()
                .map_err(capture_provider_error)?
                .to_string(),
            is_password: el
                .CachedIsPassword()
                .map_err(capture_provider_error)?
                .as_bool(),
            is_enabled: el
                .CachedIsEnabled()
                .map_err(capture_provider_error)?
                .as_bool(),
            has_text_pattern: cached_variant(&el, UIA_IsTextPatternAvailablePropertyId)
                .map_err(capture_provider_error)?
                .as_bool()
                .unwrap_or(false),
            ..Default::default()
        };
        let has_value = cached_variant(&el, UIA_IsValuePatternAvailablePropertyId)
            .map_err(capture_provider_error)?
            .as_bool()
            .unwrap_or(false);
        if has_value {
            facts.value_read_only = cached_variant(&el, UIA_ValueIsReadOnlyPropertyId)
                .map_err(capture_provider_error)?
                .as_bool();
        }
        let runtime_id = cached_variant(&el, UIA_RuntimeIdPropertyId)
            .map_err(capture_provider_error)?
            .as_i32_vec()
            .unwrap_or_default();
        let native = el
            .CachedNativeWindowHandle()
            .map_err(capture_provider_error)?;
        if facts.framework_id.eq_ignore_ascii_case("Win32") && !native.is_invalid() {
            facts.real_class = real_class(native);
            let is_edit = facts.real_class.as_deref().is_some_and(|rc| {
                WIN32_EDIT_CLASSES
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(rc))
            });
            if is_edit {
                let style = GetWindowLongPtrW(native, GWL_STYLE) as i64;
                facts.win32_read_only = Some(style & i64::from(ES_READONLY) != 0);
            }
        }

        // With range_count still 0, every rule before the range check has
        // passed exactly when classification reports NoCaret.
        match classify_capture(&facts) {
            Err(CaptureError::NoCaret) => {}
            Err(e) => return Err(e),
            Ok(_) => unreachable!("range_count 0 never classifies as success"),
        }

        let pattern: IUIAutomationTextPattern = el
            .GetCurrentPatternAs(UIA_TextPatternId)
            .map_err(|_| CaptureError::NoTextPattern)?;
        let selection = pattern.GetSelection().map_err(capture_provider_error)?;
        facts.range_count = selection.Length().map_err(capture_provider_error)?;
        let mut range = None;
        if facts.range_count == 1 {
            let r = selection.GetElement(0).map_err(capture_provider_error)?;
            facts.degenerate = r
                .CompareEndpoints(
                    TextPatternRangeEndpoint_Start,
                    &r,
                    TextPatternRangeEndpoint_End,
                )
                .map_err(capture_provider_error)?
                == 0;
            // Read the attribute at the two endpoints, not across the range:
            // Chromium evaluates a range attribute in superlinear time
            // (probe: 618 ms for 9.9k chars, >1 s for 19.9k; 2 ms collapsed).
            facts.text_attr_read_only = evidence::combine_read_only(
                endpoint_read_only(&r, TextPatternRangeEndpoint_Start),
                endpoint_read_only(&r, TextPatternRangeEndpoint_End),
            );
            if !facts.degenerate {
                facts.interior_read_only = interior_read_only(&r);
                let max = i32::try_from(2 * MAX_SELECTION_SCALARS + 1).unwrap_or(i32::MAX);
                let text = r.GetText(max).map_err(capture_provider_error)?;
                facts.text = Some(text.to_vec());
            }
            range = Some(r);
        }
        Ok(CaptureRead {
            facts,
            runtime_id,
            range,
        })
    }
}

/// Live reads for `compare_evidence`, caching the fresh element and range.
struct WinLive<'a> {
    uia: &'a Uia,
    stored: &'a IUIAutomationTextRange,
    element: Option<IUIAutomationElement>,
    fresh: Option<IUIAutomationTextRange>,
}

impl<'a> WinLive<'a> {
    fn new(uia: &'a Uia, stored: &'a IUIAutomationTextRange) -> Self {
        Self {
            uia,
            stored,
            element: None,
            fresh: None,
        }
    }

    fn fresh(&self) -> Result<&IUIAutomationTextRange, StaleReason> {
        self.fresh.as_ref().ok_or(StaleReason::SelectionChanged)
    }
}

impl LiveTarget for WinLive<'_> {
    fn foreground_hwnd(&mut self) -> usize {
        // SAFETY: plain Win32 query.
        unsafe { GetForegroundWindow() }.0 as usize
    }

    fn process_identity(&mut self, hwnd: usize) -> Result<(u32, u64), StaleReason> {
        let (pid, _) = window_pid_tid(HWND(hwnd as *mut _));
        let created = process_created(pid).ok_or(StaleReason::ProcessChanged)?;
        Ok((pid, created))
    }

    fn gui_focus_hwnd(&mut self, thread_id: u32) -> usize {
        gui_focus_hwnd(thread_id)
    }

    fn focused_element(&mut self) -> Result<(Vec<i32>, u32), StaleReason> {
        // SAFETY: COM calls on objects owned by this thread.
        unsafe {
            let el = self
                .uia
                .automation
                .GetFocusedElementBuildCache(&self.uia.focus_cache)
                .map_err(stale_provider_error)?;
            let runtime_id = cached_variant(&el, UIA_RuntimeIdPropertyId)
                .map_err(stale_provider_error)?
                .as_i32_vec()
                .unwrap_or_default();
            let pid = el.CachedProcessId().map_err(stale_provider_error)? as u32;
            self.element = Some(el);
            Ok((runtime_id, pid))
        }
    }

    fn selection_count(&mut self) -> Result<i32, StaleReason> {
        let el = self.element.as_ref().ok_or(StaleReason::FocusChanged)?;
        // SAFETY: COM calls on objects owned by this thread.
        unsafe {
            let pattern: IUIAutomationTextPattern = el
                .GetCurrentPatternAs(UIA_TextPatternId)
                .map_err(stale_provider_error)?;
            let selection = pattern.GetSelection().map_err(stale_provider_error)?;
            let count = selection.Length().map_err(stale_provider_error)?;
            if count >= 1 {
                self.fresh = Some(selection.GetElement(0).map_err(stale_provider_error)?);
            }
            Ok(count)
        }
    }

    fn endpoint_deltas(&mut self) -> Result<(i32, i32), StaleReason> {
        let fresh = self.fresh()?;
        // SAFETY: COM calls on objects owned by this thread.
        unsafe {
            let start = fresh
                .CompareEndpoints(
                    TextPatternRangeEndpoint_Start,
                    self.stored,
                    TextPatternRangeEndpoint_Start,
                )
                .map_err(stale_provider_error)?;
            let end = fresh
                .CompareEndpoints(
                    TextPatternRangeEndpoint_End,
                    self.stored,
                    TextPatternRangeEndpoint_End,
                )
                .map_err(stale_provider_error)?;
            Ok((start, end))
        }
    }

    fn selection_text(&mut self, max_len: i32) -> Result<Vec<u16>, StaleReason> {
        let fresh = self.fresh()?;
        // SAFETY: COM call on an object owned by this thread.
        let text = unsafe { fresh.GetText(max_len) }.map_err(stale_provider_error)?;
        Ok(text.to_vec())
    }

    fn selection_degenerate(&mut self) -> Result<bool, StaleReason> {
        let fresh = self.fresh()?;
        // SAFETY: COM call on an object owned by this thread.
        let delta = unsafe {
            fresh.CompareEndpoints(
                TextPatternRangeEndpoint_Start,
                fresh,
                TextPatternRangeEndpoint_End,
            )
        }
        .map_err(stale_provider_error)?;
        Ok(delta == 0)
    }
}

fn worker_main(rx: Receiver<Cmd>, ready: Sender<u32>) {
    let mut msg = MSG::default();
    // SAFETY: creates this thread's message queue before reporting its id,
    // so PostThreadMessageW cannot race the queue creation.
    unsafe {
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
    }
    let _ = ready.send(unsafe { GetCurrentThreadId() });

    // SAFETY: balanced by CoUninitialize below when it succeeded.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let uia = if com.is_ok() {
        create_uia().map_err(|e| format!("UI Automation unavailable: {}", hresult_text(&e)))
    } else {
        Err(format!("CoInitializeEx failed: {:#010x}", com.0))
    };
    if let Err(e) = &uia {
        warn!("[selection] {e}");
    }
    let mut worker = Worker {
        uia,
        target: None,
        next_id: 0,
        own_pid: unsafe { GetCurrentProcessId() },
    };

    let mut shutdown_ack: Option<Sender<()>> = None;
    'pump: loop {
        // SAFETY: standard message loop on the worker thread.
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if got.0 == 0 || got.0 == -1 {
            break;
        }
        if msg.message != WM_WAKE {
            unsafe {
                DispatchMessageW(&msg);
            }
            continue;
        }
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::Capture { session, reply } => {
                    let result = worker.capture(session);
                    reply.send(result);
                }
                Cmd::Validate {
                    id,
                    deadline,
                    reply,
                } => {
                    // A request whose caller already gave up is not worth
                    // a cross-process round trip.
                    let result = if Instant::now() >= deadline {
                        Err(StaleReason::Unresponsive)
                    } else {
                        worker.validate(id, deadline)
                    };
                    let _ = reply.send(result);
                }
                Cmd::PrepareInjection {
                    id,
                    deadline,
                    reply,
                } => {
                    drain_messages();
                    let result = if Instant::now() >= deadline {
                        Err(StaleReason::Unresponsive)
                    } else if let Some(tracked) = worker.target.as_mut().filter(|t| t.id == id) {
                        evidence::finish_validation(
                            Ok(()),
                            true,
                            &tracked.flag,
                            || {},
                            || {
                                unhook_all(&mut tracked.hooks);
                            },
                        )
                    } else {
                        Err(StaleReason::Released)
                    };
                    let _ = reply.send(result);
                }
                Cmd::Verify {
                    id,
                    text,
                    deadline,
                    reply,
                } => {
                    let result = Instant::now() < deadline && worker.verify(id, &text, deadline);
                    let _ = reply.send(result);
                }
                Cmd::Release(id) => {
                    if worker.target.as_ref().is_some_and(|t| t.id == id) {
                        worker.release_current(None);
                    }
                }
                Cmd::Shutdown(ack) => {
                    shutdown_ack = Some(ack);
                    break 'pump;
                }
            }
        }
    }

    worker.release_current(Some(InvalidationReason::Shutdown));
    drop(worker);
    if com.is_ok() {
        // SAFETY: balances the successful CoInitializeEx; all COM objects
        // owned by this thread were dropped above.
        unsafe { CoUninitialize() };
    }
    if let Some(ack) = shutdown_ack {
        let _ = ack.send(());
    }
}

// --- commit platform ---------------------------------------------------------

/// `CommitPlatform` over the selection worker, the strict clipboard
/// transaction and the atomic chord.
pub(super) struct WinCommitPlatform {
    tx: Option<StrictClipboardTx>,
}

impl WinCommitPlatform {
    pub(super) fn new() -> Self {
        Self { tx: None }
    }
}

fn begin_error(e: PublishError) -> BeginClipboardError {
    match e {
        PublishError::Lossy(lossy) => BeginClipboardError::Refused(
            RefuseReason::ClipboardNotPreservable(lossy),
            ClipboardSettlement::Untouched,
        ),
        PublishError::Busy(_) => BeginClipboardError::Refused(
            RefuseReason::ClipboardBusy,
            ClipboardSettlement::Untouched,
        ),
        PublishError::Abandoned => BeginClipboardError::Refused(
            RefuseReason::ClipboardTimeout,
            ClipboardSettlement::Untouched,
        ),
        // A late publish restores itself, but whether it ran is unknown.
        PublishError::Timeout => BeginClipboardError::Refused(
            RefuseReason::ClipboardTimeout,
            ClipboardSettlement::Unknown,
        ),
        PublishError::Failed { error, restore } => {
            BeginClipboardError::Failed(FailReason::PublishFailed(error), restore)
        }
    }
}

impl CommitPlatform for WinCommitPlatform {
    fn validate(&mut self, id: TargetId, deadline: Instant) -> Result<(), StaleReason> {
        request(
            |reply| Cmd::Validate {
                id,
                deadline,
                reply,
            },
            deadline,
        )
        .unwrap_or(Err(StaleReason::Unresponsive))
    }

    fn prepare_injection(&mut self, id: TargetId, deadline: Instant) -> Result<(), StaleReason> {
        request(
            |reply| Cmd::PrepareInjection {
                id,
                deadline,
                reply,
            },
            deadline,
        )
        .unwrap_or(Err(StaleReason::Unresponsive))
    }

    fn begin_clipboard(
        &mut self,
        text: &str,
        after_success: ClipboardHandling,
    ) -> Result<(), BeginClipboardError> {
        let tx = StrictClipboardTx::publish(text, after_success).map_err(begin_error)?;
        self.tx = Some(tx);
        #[cfg(test)]
        crate::paste_tx::diag::run_after_publish();
        Ok(())
    }

    fn clipboard_still_owned(&self) -> bool {
        self.tx.as_ref().is_some_and(StrictClipboardTx::still_owned)
    }

    fn clipboard_free(&self) -> bool {
        StrictClipboardTx::clipboard_free()
    }

    fn modifiers_held(&self) -> bool {
        crate::input::modifiers_held()
    }

    fn inject_paste(&mut self) -> ChordResult {
        let Some(tx) = self.tx.as_ref() else {
            return ChordResult::NotSent("no clipboard transaction".into());
        };
        tx.mark_injecting();
        let (sent, total, error) = crate::input::send_ctrl_v_atomic();
        let (result, _) = classify_chord(sent, total, error);
        release_partial_chord(chord_release_keys(sent, total));
        match &result {
            ChordResult::NotSent(m) => {
                tx.mark_injection_failed();
                warn!("[selection] paste chord not sent: {m}");
            }
            ChordResult::Partial(m) => warn!("[selection] paste chord partially sent: {m}"),
            ChordResult::Sent => {}
        }
        result
    }

    fn abort_clipboard(&mut self) -> ClipboardSettlement {
        self.tx
            .as_ref()
            .map(StrictClipboardTx::abort)
            .unwrap_or(ClipboardSettlement::Untouched)
    }

    fn await_settlement(&mut self) -> Option<StrictSettlement> {
        self.tx
            .as_ref()?
            .wait_settled(StrictClipboardTx::settle_timeout())
    }

    fn verify_inserted(&mut self, id: TargetId, text: &str, deadline: Instant) -> bool {
        let text = text.to_string();
        request(
            |reply| Cmd::Verify {
                id,
                text,
                deadline,
                reply,
            },
            deadline,
        )
        .unwrap_or(false)
    }
}
