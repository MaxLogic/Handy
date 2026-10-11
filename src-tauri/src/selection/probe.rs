//! Live feasibility probe support (test-only, Windows). Launches its own
//! target windows, drives them through UI Automation, and closes them again.
//! It never interacts with windows it did not launch.

#![allow(clippy::too_many_lines)]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{w, BSTR, PCWSTR};
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Foundation::{HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardSequenceNumber,
    OpenClipboard,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, LoadLibraryW};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_BOOL};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationTextRange, IUIAutomationValuePattern, TextPatternRangeEndpoint_End,
    TextPatternRangeEndpoint_Start, UIA_IsReadOnlyAttributeId, UIA_TextPatternId,
    UIA_ValuePatternId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, MOUSEEVENTF_MOVE, MOUSEINPUT, VIRTUAL_KEY, VK_CONTROL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    EnumWindows, GetClassNameW, GetForegroundWindow, GetMessageW, GetWindowLongPtrW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindow, IsWindowVisible, PostMessageW,
    PostQuitMessage, RealGetWindowClassW, RegisterClassW, SendMessageW, SetForegroundWindow,
    SetWindowLongPtrW, ShowWindow, TranslateMessage, ES_AUTOVSCROLL, ES_MULTILINE, GWLP_USERDATA,
    MSG, SW_RESTORE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_DESTROY, WM_SETFOCUS,
    WM_SETTEXT, WNDCLASSW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE, WS_VSCROLL,
};

/// Initial document of every probe target.
pub(super) const DOC: &str = "alpha SELECTME-one\r\nSELECTME-two omega";
pub(super) const SEL_START: &str = "SELECTME-one";
pub(super) const SEL_END: &str = "SELECTME-two";
const EM_SETSEL: u32 = 0x00B1;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// --- edit host (runs in a child process) -------------------------------------

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SETFOCUS => {
            let child = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if child != 0 {
                let _ = SetFocus(Some(HWND(child as *mut _)));
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != 0 {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Creates "HandyProbeHost" (with a multiline edit of `class`) and a plain
/// "HandyProbeOther" window, then pumps until the host closes.
pub(super) fn run_edit_host(class: &str) {
    unsafe {
        if !class.eq_ignore_ascii_case("EDIT") {
            let _ = LoadLibraryW(w!("msftedit.dll"));
        }
        let hinstance = HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap().0);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(host_proc),
            hInstance: hinstance,
            lpszClassName: w!("HandyProbeHostClass"),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let title = wide(&format!("HandyProbeHost {class}"));
        let host = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("HandyProbeHostClass"),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            700,
            400,
            None,
            None,
            Some(hinstance),
            None,
        )
        .expect("host window");
        let class_w = wide(class);
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_w.as_ptr()),
            PCWSTR::null(),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL) as u32),
            10,
            10,
            660,
            320,
            Some(host),
            None,
            Some(hinstance),
            None,
        )
        .expect("edit control");
        SetWindowLongPtrW(host, GWLP_USERDATA, edit.0 as isize);
        let doc = wide(DOC);
        SendMessageW(edit, WM_SETTEXT, None, Some(LPARAM(doc.as_ptr() as isize)));
        let start = DOC.find(SEL_START).unwrap();
        let end = DOC.find(SEL_END).unwrap() + SEL_END.len();
        SendMessageW(
            edit,
            EM_SETSEL,
            Some(WPARAM(start)),
            Some(LPARAM(end as isize)),
        );
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("HandyProbeHostClass"),
            w!("HandyProbeOther"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            820,
            100,
            300,
            200,
            None,
            None,
            Some(hinstance),
            None,
        );
        let _ = SetFocus(Some(edit));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// --- window discovery and foreground -------------------------------------------

#[derive(Clone, Debug)]
pub(super) struct Win {
    pub hwnd: usize,
    pub pid: u32,
    pub class: String,
    pub title: String,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
    let out = &mut *(lp.0 as *mut Vec<Win>);
    if IsWindowVisible(hwnd).as_bool() {
        let mut class = [0u16; 256];
        let n = GetClassNameW(hwnd, &mut class) as usize;
        let mut title = [0u16; 512];
        let t = GetWindowTextW(hwnd, &mut title) as usize;
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        out.push(Win {
            hwnd: hwnd.0 as usize,
            pid,
            class: String::from_utf16_lossy(&class[..n]),
            title: String::from_utf16_lossy(&title[..t]),
        });
    }
    true.into()
}

pub(super) fn top_windows() -> Vec<Win> {
    let mut out: Vec<Win> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

pub(super) fn wait_window(pred: impl Fn(&Win) -> bool, timeout: Duration) -> Option<Win> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(w) = top_windows().into_iter().find(|w| pred(w)) {
            OWNED_WINDOWS.lock().unwrap().push((w.hwnd, w.pid));
            return Some(w);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

pub(super) fn hwnd(h: usize) -> HWND {
    HWND(h as *mut _)
}

pub(super) fn foreground() -> usize {
    unsafe { GetForegroundWindow().0 as usize }
}

static OWNED_WINDOWS: Mutex<Vec<(usize, u32)>> = Mutex::new(Vec::new());
static INITIAL_ACTIVATION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub(super) fn check_owned_foreground() -> Result<(), String> {
    if !owned_window(foreground()) {
        return Err("probe stopped: foreground moved outside its own windows".into());
    }
    Ok(())
}

fn owned_window(target: usize) -> bool {
    let mut pid = 0;
    // SAFETY: read-only window query, including for an expired HWND.
    unsafe {
        GetWindowThreadProcessId(hwnd(target), Some(&mut pid));
    }
    OWNED_WINDOWS.lock().unwrap().contains(&(target, pid))
}

/// Panic-safe cleanup of only windows discovered by the probe. No activation
/// or keyboard input occurs on cleanup.
pub(super) struct ProbeWindows;

impl Drop for ProbeWindows {
    fn drop(&mut self) {
        let targets = OWNED_WINDOWS.lock().unwrap().clone();
        for (target, _) in targets {
            close_window(target);
        }
        OWNED_WINDOWS.lock().unwrap().clear();
    }
}

/// Brings one of the probe's own windows to the foreground.
pub(super) fn force_foreground(target: usize) -> bool {
    use std::sync::atomic::Ordering;
    if !owned_window(target) {
        return false;
    }
    let initial = INITIAL_ACTIVATION.swap(false, Ordering::SeqCst);
    assert!(
        initial || owned_window(foreground()),
        "probe stopped: user changed foreground"
    );
    let h = hwnd(target);
    unsafe {
        let _ = ShowWindow(h, SW_RESTORE);
        let _ = ShowWindow(h, SW_SHOW);
        // A zero-distance mouse move makes this process the last input
        // source, which lets SetForegroundWindow succeed.
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(h);
        if foreground() != target {
            let fg = GetForegroundWindow();
            let fg_tid = GetWindowThreadProcessId(fg, None);
            let me = GetCurrentThreadId();
            let attached = fg_tid != 0 && AttachThreadInput(me, fg_tid, true).as_bool();
            let _ = BringWindowToTop(h);
            let _ = SetForegroundWindow(h);
            if attached {
                let _ = AttachThreadInput(me, fg_tid, false);
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if foreground() == target {
            std::thread::sleep(Duration::from_millis(300));
            return foreground() == target;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

pub(super) fn close_window(h: usize) {
    if !owned_window(h) {
        return;
    }
    unsafe {
        let _ = PostMessageW(Some(hwnd(h)), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

pub(super) fn window_alive(h: usize) -> bool {
    unsafe { IsWindow(Some(hwnd(h))).as_bool() }
}

/// Sends Ctrl+<key> only while `target` is the foreground window.
pub(super) fn ctrl_key_to(target: usize, vk: u16) -> bool {
    if foreground() != target {
        return false;
    }
    let key = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [
        key(VK_CONTROL, false),
        key(VIRTUAL_KEY(vk), false),
        key(VIRTUAL_KEY(vk), true),
        key(VK_CONTROL, true),
    ];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) == 4 }
}

// --- process launch --------------------------------------------------------------

pub(super) fn spawn_edit_host(class: &str) -> ProbeChild {
    ProbeChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "selection::tests::probe_edit_host",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("HANDY_PROBE_EDIT_HOST", class)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn edit host"),
    )
}

pub(super) struct ProbeChild(Child);

impl std::ops::Deref for ProbeChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for ProbeChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for ProbeChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            close_window_of_pid(self.0.id());
            wait_child(&mut self.0);
        }
    }
}

pub(super) fn temp_dir() -> PathBuf {
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("handy-probe-{}-{run}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(super) fn file_url(path: &Path) -> String {
    format!("file:///{}", path.display().to_string().replace('\\', "/"))
}

pub(super) fn process_running(image: &str) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {image}"), "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .to_ascii_lowercase()
            .contains(&image.to_ascii_lowercase()),
        Err(_) => true,
    }
}

pub(super) fn process_executable(pid: u32) -> String {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return "0:none-or-windowless".into();
    }
    // SAFETY: query-only handle; the output buffer is bounded and the handle
    // is closed on both success and failure.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return format!("{pid}:unavailable");
        };
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(handle);
        if result.is_err() {
            return format!("{pid}:unavailable");
        }
        let full = String::from_utf16_lossy(&buffer[..length as usize]);
        let name = Path::new(&full)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&full);
        format!("{pid}:{name}")
    }
}

/// Kills `image` processes whose command line contains `needle` (the probe's
/// private profile path), so the user's own processes are never touched.
pub(super) fn kill_processes_with_arg(image: &str, needle: &str) {
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='{image}'\" | \
         Where-Object {{ $_.CommandLine -and $_.CommandLine.Contains('{}') }} | \
         ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}",
        needle.replace('\'', "''")
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

// --- probe-side UI Automation (independent of the selection worker) ---------------

pub(super) struct ProbeUia {
    pub uia: IUIAutomation,
}

impl ProbeUia {
    pub fn new() -> Self {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            Self {
                uia: CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER).unwrap(),
            }
        }
    }

    fn focused(&self) -> windows::core::Result<IUIAutomationElement> {
        check_owned_foreground().expect("only probe-owned foreground controls may be accessed");
        unsafe { self.uia.GetFocusedElement() }
    }

    fn pattern(&self) -> windows::core::Result<IUIAutomationTextPattern> {
        unsafe { self.focused()?.GetCurrentPatternAs(UIA_TextPatternId) }
    }

    fn find(&self, needle: &str) -> windows::core::Result<IUIAutomationTextRange> {
        unsafe {
            self.pattern()?
                .DocumentRange()?
                .FindText(&BSTR::from(needle), false, false)
        }
    }

    /// Identity evidence of the focused element, for the report.
    pub fn describe(&self) -> String {
        unsafe {
            match self.focused() {
                Ok(el) => {
                    let native = el.CurrentNativeWindowHandle().unwrap_or_default();
                    let mut buf = [0u16; 256];
                    let n = if native.is_invalid() {
                        0
                    } else {
                        RealGetWindowClassW(native, &mut buf) as usize
                    };
                    let rid = match el.GetRuntimeId() {
                        Ok(psa) if !psa.is_null() => {
                            let lower = SafeArrayGetLBound(psa, 1).unwrap_or(0);
                            let upper = SafeArrayGetUBound(psa, 1).unwrap_or(-1);
                            let _ = SafeArrayDestroy(psa);
                            format!("{} ints", upper - lower + 1)
                        }
                        _ => "missing".to_string(),
                    };
                    format!(
                        "framework={} class={} real_class={} control_type={} runtime_id={}",
                        el.CurrentFrameworkId().unwrap_or_default(),
                        el.CurrentClassName().unwrap_or_default(),
                        String::from_utf16_lossy(&buf[..n]),
                        el.CurrentControlType().map(|c| c.0).unwrap_or(0),
                        rid
                    )
                }
                Err(e) => format!("no focused element: {e}"),
            }
        }
    }

    /// Selects from the start of `from` to the end of `to`.
    pub fn select_between(&self, from: &str, to: &str) -> windows::core::Result<()> {
        unsafe {
            let a = self.find(from)?;
            let b = self.find(to)?;
            a.MoveEndpointByRange(
                TextPatternRangeEndpoint_End,
                &b,
                TextPatternRangeEndpoint_End,
            )?;
            a.Select()
        }
    }

    pub fn select_word(&self, word: &str) -> windows::core::Result<()> {
        unsafe { self.find(word)?.Select() }
    }

    /// Places a collapsed caret just before `word`.
    pub fn caret_before(&self, word: &str) -> windows::core::Result<()> {
        unsafe {
            let r = self.find(word)?;
            r.MoveEndpointByRange(
                TextPatternRangeEndpoint_End,
                &r,
                TextPatternRangeEndpoint_Start,
            )?;
            r.Select()
        }
    }

    /// IsReadOnly as the provider reports it for `word`, and whether a
    /// read-only run is found between `from` and `to`.
    pub fn read_only_report(&self, word: &str, from: &str, to: &str) -> String {
        unsafe {
            let attr = self
                .find(word)
                .and_then(|r| r.GetAttributeValue(UIA_IsReadOnlyAttributeId))
                .map(|mut v| {
                    let vt = v.Anonymous.Anonymous.vt.0;
                    let b = v.Anonymous.Anonymous.Anonymous.boolVal.0;
                    let _ = VariantClear(&mut v);
                    format!("vt={vt} bool={b}")
                });
            let scan = (|| {
                let a = self.find(from)?;
                let b = self.find(to)?;
                a.MoveEndpointByRange(
                    TextPatternRangeEndpoint_End,
                    &b,
                    TextPatternRangeEndpoint_End,
                )?;
                let mut v = VARIANT::default();
                (*v.Anonymous.Anonymous).vt = VT_BOOL;
                (*v.Anonymous.Anonymous).Anonymous.boolVal = VARIANT_TRUE;
                a.FindAttribute(UIA_IsReadOnlyAttributeId, &v, false)
                    .and_then(|r| r.GetText(-1))
                    .map(|t| t.to_string())
            })();
            format!("{word:?} IsReadOnly {attr:?}; scan {from}..{to}: {scan:?}")
        }
    }

    pub fn doc_text(&self) -> String {
        unsafe {
            self.pattern()
                .and_then(|p| p.DocumentRange())
                .and_then(|r| r.GetText(-1))
                .map(|b| b.to_string())
                .unwrap_or_else(|e| format!("<unreadable: {e}>"))
        }
    }

    /// Replaces the whole document: ValuePattern, else WM_SETTEXT.
    pub fn set_doc(&self, text: &str) -> Result<(), String> {
        unsafe {
            let el = self.focused().map_err(|e| e.to_string())?;
            if let Ok(v) = el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
                if v.SetValue(&BSTR::from(text)).is_ok() {
                    return Ok(());
                }
            }
            let native = el.CurrentNativeWindowHandle().map_err(|e| e.to_string())?;
            if native.is_invalid() {
                return Err("no ValuePattern and no native window".into());
            }
            let w = wide(text);
            SendMessageW(native, WM_SETTEXT, None, Some(LPARAM(w.as_ptr() as isize)));
            Ok(())
        }
    }
}

// --- clipboard digest ----------------------------------------------------------------

/// Sequence number plus (format, hash of bytes or None if not global memory).
pub(super) fn clipboard_digest() -> (u32, Vec<(u32, Option<u64>)>) {
    unsafe {
        let seq = GetClipboardSequenceNumber();
        let mut formats = Vec::new();
        for _ in 0..20 {
            if OpenClipboard(None).is_ok() {
                let mut f = 0u32;
                loop {
                    f = EnumClipboardFormats(f);
                    if f == 0 {
                        break;
                    }
                    // Only hash plain-memory formats; GDI handles stay None.
                    let hash = if matches!(f, 2 | 3 | 9 | 14 | 0x80..=0x8E)
                        || (0x200..=0x3FF).contains(&f)
                    {
                        None
                    } else {
                        GetClipboardData(f).ok().and_then(|h| {
                            let hg = HGLOBAL(h.0);
                            let size = GlobalSize(hg);
                            if size == 0 {
                                return None;
                            }
                            let ptr = GlobalLock(hg) as *const u8;
                            if ptr.is_null() {
                                return None;
                            }
                            let mut hasher = DefaultHasher::new();
                            std::slice::from_raw_parts(ptr, size).hash(&mut hasher);
                            let _ = GlobalUnlock(hg);
                            Some(hasher.finish())
                        })
                    };
                    formats.push((f, hash));
                }
                let _ = CloseClipboard();
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // Restoration may reorder formats (the bitmap is written last).
        formats.sort();
        (seq, formats)
    }
}

// --- capture helper ----------------------------------------------------------------

/// Runs `begin_capture` and waits for its single report.
pub(super) fn capture(session: u64) -> (Result<super::Captured, super::CaptureError>, Duration) {
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    let _ticket = super::begin_capture(super::SessionId(session), move |r| {
        let _ = tx.send((r, started.elapsed()));
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("on_done within the capture bound")
}

/// Closes every visible top-level window of `pid` (probe-launched only).
pub(super) fn close_window_of_pid(pid: u32) {
    for w in top_windows().into_iter().filter(|w| w.pid == pid) {
        close_window(w.hwnd);
    }
}

/// Waits up to 5 s for a probe child to exit, then kills it.
pub(super) fn wait_child(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

// --- clipboard fixtures (only used when the clipboard was empty at start) -------

/// One fixture format: raw global-memory bytes, or an empty enhanced metafile.
pub(super) enum ClipItem {
    Bytes(u32, Vec<u8>),
    EnhMetafile,
}

pub(super) fn registered_format(name: &str) -> u32 {
    let w = wide(name);
    unsafe { windows::Win32::System::DataExchange::RegisterClipboardFormatW(PCWSTR(w.as_ptr())) }
}

/// A 2x2 32-bpp CF_DIB.
pub(super) fn tiny_dib() -> Vec<u8> {
    let mut v = Vec::new();
    for x in [40i32, 2, 2] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&32u16.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&[0u8; 16]);
    v.extend_from_slice(&[0x10, 0x20, 0x30, 0xFF].repeat(4));
    v
}

pub(super) fn utf16_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(|u| u.to_le_bytes())
        .collect()
}

/// Human-gated native F5 case: the reader holds the clipboard, then requests
/// the unrendered format only once the owner is retrying abort restoration.
pub(super) fn abort_reader_during_settlement() -> Result<String, String> {
    use crate::paste_tx::{diag, ClipboardSettlement, StrictClipboardTx};
    use crate::settings::ClipboardHandling;
    use windows::Win32::System::DataExchange::GetClipboardData;
    if std::env::var("HANDY_PROBE_DESKTOP_AVAILABLE").as_deref() != Ok("1") {
        return Err("desktop lease is required".into());
    }
    check_owned_foreground()?;
    if !diag::DECLINE.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("abort-reader case requires D1 ON".into());
    }
    set_clipboard(&[ClipItem::Bytes(13, utf16_bytes("PROBE-ABORT-ORIGINAL"))])?;
    let before = clipboard_digest().1;
    let _ = diag::take();
    let tx = StrictClipboardTx::publish("PROBE-ABORT-REPLACEMENT", ClipboardHandling::DontModify)
        .map_err(|e| format!("publish: {e:?}"))?;
    let (opened_tx, opened_rx) = std::sync::mpsc::channel();
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        // SAFETY: this thread balances its own OpenClipboard/CloseClipboard.
        unsafe {
            let opened = OpenClipboard(None).map_err(|e| e.to_string());
            let _ = opened_tx.send(opened.clone());
            if opened.is_err() {
                return;
            }
            let read = request_rx
                .recv_timeout(Duration::from_secs(2))
                .map(|()| GetClipboardData(13).is_ok());
            let _ = CloseClipboard();
            let _ = done_tx.send(read);
        }
    });
    opened_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|e| e.to_string())??;
    struct RetryHook;
    impl Drop for RetryHook {
        fn drop(&mut self) {
            diag::ON_CLIPBOARD_RETRY.lock().unwrap().take();
        }
    }
    let _hook = RetryHook;
    *diag::ON_CLIPBOARD_RETRY.lock().unwrap() = Some(Box::new(move || {
        let _ = request_tx.send(());
    }));
    let settlement = tx.abort();
    let rendered = done_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|e| format!("reader did not finish: {e}"))?
        .map_err(|e| format!("reader did not reach abort retry: {e}"))?;
    reader.join().map_err(|_| "reader panicked".to_string())?;
    let events = diag::take();
    let declined = events.iter().any(|e| {
        matches!(
            e,
            diag::Event::Render {
                rendered: false,
                after_injection: false,
                ..
            }
        )
    });
    if rendered || !declined || settlement != ClipboardSettlement::Restored {
        return Err(format!(
            "rendered={rendered}; declined={declined}; settlement={settlement:?}"
        ));
    }
    if clipboard_digest().1 != before {
        return Err("original clipboard formats/bytes were not restored".into());
    }
    Ok("reader requested format during abort settlement; declined=true; original clipboard restored; no injection".into())
}

/// Replaces the clipboard with `items` (empty slice empties it).
pub(super) fn set_clipboard(items: &[ClipItem]) -> Result<(), String> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::Graphics::Gdi::{CloseEnhMetaFile, CreateEnhMetaFileW, DeleteEnhMetaFile};
    use windows::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GMEM_MOVEABLE};
    unsafe {
        let mut opened = false;
        for _ in 0..20 {
            if OpenClipboard(None).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if !opened {
            return Err("OpenClipboard failed".into());
        }
        let before = GetClipboardSequenceNumber();
        if !crate::paste_tx::diag::session_current(before) {
            let _ = CloseClipboard();
            return Err("probe stopped: external clipboard copy must be preserved".into());
        }
        let mut result = EmptyClipboard().map_err(|e| e.to_string());
        for item in items {
            if result.is_err() {
                break;
            }
            result = match item {
                ClipItem::Bytes(format, bytes) => {
                    let hg = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(|e| e.to_string());
                    hg.and_then(|hg| {
                        let ptr = GlobalLock(hg) as *mut u8;
                        if ptr.is_null() {
                            let _ = GlobalFree(Some(hg));
                            return Err("GlobalLock failed".into());
                        }
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                        let _ = GlobalUnlock(hg);
                        SetClipboardData(*format, Some(HANDLE(hg.0)))
                            .map(|_| ())
                            .map_err(|e| {
                                let _ = GlobalFree(Some(hg));
                                e.to_string()
                            })
                    })
                }
                ClipItem::EnhMetafile => {
                    let hdc = CreateEnhMetaFileW(None, PCWSTR::null(), None, PCWSTR::null());
                    let emf = CloseEnhMetaFile(hdc);
                    if emf.is_invalid() {
                        Err("CreateEnhMetaFile failed".into())
                    } else {
                        SetClipboardData(14, Some(HANDLE(emf.0)))
                            .map(|_| ())
                            .map_err(|e| {
                                let _ = DeleteEnhMetaFile(Some(emf));
                                e.to_string()
                            })
                    }
                }
            };
        }
        crate::paste_tx::diag::note_locked_change(before, GetClipboardSequenceNumber());
        let _ = CloseClipboard();
        result
    }
}
