//! Explicitly leased, ignored native proof against a coordinator-owned fixture.
//! This module never launches, activates, saves, or closes a window.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessIdOfThread, GetProcessTimes, OpenProcess, OpenThread,
    ResumeThread, SuspendThread, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
    RealGetWindowClassW, SendMessageTimeoutW, GA_ROOT, GUITHREADINFO, SMTO_ABORTIFHUNG, SMTO_BLOCK,
};

use super::{probe, CaptureError, Captured, CapturedText, CommitOutcome, CommitRequest, SessionId};

const DOC: &str = "Alpha zażółć gęślą jaźń.\r\nSecond paragraph.";
const MAX_DOCUMENT: usize = 4096;

fn environment(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("explicit fixture authorization requires {name}"))
}

fn window_pid_tid(hwnd: HWND) -> (u32, u32) {
    let mut pid = 0;
    // SAFETY: a local initialized out parameter; query only.
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid, tid)
}

fn class(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    // SAFETY: the API receives a correctly bounded local output slice.
    let len = unsafe { RealGetWindowClassW(hwnd, &mut buffer) } as usize;
    String::from_utf16_lossy(&buffer[..len.min(buffer.len())])
}

fn process_created(handle: HANDLE) -> Result<u64, String> {
    let (mut birth, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: retained query handle, four disjoint initialized local buffers.
    unsafe { GetProcessTimes(handle, &mut birth, &mut exit, &mut kernel, &mut user) }
        .map_err(|_| "fixture process identity unavailable".to_string())?;
    Ok((u64::from(birth.dwHighDateTime) << 32) | u64::from(birth.dwLowDateTime))
}

struct Process(HANDLE);

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: exclusively owned successful OpenProcess handle, closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct Fixture {
    pid: u32,
    tid: u32,
    main: HWND,
    editor: HWND,
    process: Process,
    created: u64,
    lease: PathBuf,
    path: PathBuf,
    nonce: String,
    document: isize,
    original_anchor: isize,
    original_caret: isize,
    undo_open: bool,
    undo_pending: bool,
    initialized: bool,
}

/// A synchronous commit may share its cancellation callback across threads.
/// Copy HWND/HANDLE values as integers for query-only Win32 calls; their owning
/// Fixture remains borrowed and alive until commit returns. This does not make
/// Fixture or any UI/COM object Send/Sync.
struct Authorization {
    lease: PathBuf,
    fixture_path: PathBuf,
    nonce: String,
    pid: u32,
    tid: u32,
    main: usize,
    editor: usize,
    process: usize,
    created: u64,
}

impl Authorization {
    fn current(&self) -> bool {
        let check = || -> Option<()> {
            let bytes = std::fs::read(&self.lease).ok()?;
            if bytes.len() > 32768 {
                return None;
            }
            let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            let expiry = value["expiresAtMs"].as_u64()?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_millis();
            if u128::from(expiry) <= now
                || value["targetPid"].as_u64() != Some(u64::from(self.pid))
                || value["ownershipNonce"].as_str() != Some(self.nonce.as_str())
                || PathBuf::from(value["fixturePath"].as_str()?)
                    .canonicalize()
                    .ok()?
                    != self.fixture_path
            {
                return None;
            }
            let main = HWND(self.main as *mut _);
            let editor = HWND(self.editor as *mut _);
            let process = HANDLE(self.process as *mut _);
            let mut info = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            let mut exit_code = 0;
            // SAFETY: query-only calls using a handle retained by Fixture for
            // this synchronous operation and properly initialized local buffers.
            // No GUI message or foreground input occurs from this callback.
            unsafe {
                GetGUIThreadInfo(self.tid, &mut info).ok()?;
                GetExitCodeProcess(process, &mut exit_code).ok()?;
                if GetForegroundWindow() != main
                    || info.hwndFocus != editor
                    || window_pid_tid(main) != (self.pid, self.tid)
                    || window_pid_tid(editor) != (self.pid, self.tid)
                    || GetAncestor(editor, GA_ROOT) != main
                    || class(main) != "Notepad++"
                    || class(editor) != "Scintilla"
                    || exit_code != 259
                    || process_created(process).ok()? != self.created
                {
                    return None;
                }
            }
            let after = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_millis();
            (after < u128::from(expiry)).then_some(())
        };
        check().is_some()
    }
}

impl Fixture {
    fn authorization(&self) -> Authorization {
        Authorization {
            lease: self.lease.clone(),
            fixture_path: self.path.clone(),
            nonce: self.nonce.clone(),
            pid: self.pid,
            tid: self.tid,
            main: self.main.0 as usize,
            editor: self.editor.0 as usize,
            process: self.process.0 .0 as usize,
            created: self.created,
        }
    }
    fn open() -> Self {
        let pid = environment("HANDY_NPP_PID")
            .parse::<u32>()
            .expect("decimal fixture PID");
        let main = HWND(
            environment("HANDY_NPP_MAIN_HWND")
                .parse::<usize>()
                .expect("decimal main HWND") as *mut _,
        );
        let editor = HWND(
            environment("HANDY_NPP_EDITOR_HWND")
                .parse::<usize>()
                .expect("decimal editor HWND") as *mut _,
        );
        let (_, tid) = window_pid_tid(main);
        // SAFETY: query/read-only rights; the owned handle is closed by Drop.
        let process = Process(
            unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                    false,
                    pid,
                )
            }
            .expect("owned fixture query/read handle"),
        );
        let created = process_created(process.0).expect("fixture creation identity");
        let mut result = Self {
            pid,
            tid,
            main,
            editor,
            process,
            created,
            lease: PathBuf::from(environment("HANDY_NPP_LEASE")),
            path: PathBuf::from(environment("HANDY_NPP_FIXTURE"))
                .canonicalize()
                .expect("fixture path"),
            nonce: environment("HANDY_NPP_OWNERSHIP_NONCE"),
            document: 0,
            original_anchor: 0,
            original_caret: 0,
            undo_open: false,
            undo_pending: false,
            initialized: false,
        };
        assert!(
            result.nonce.len() >= 16,
            "explicit ownership nonce must be unguessable"
        );
        result.guard().expect("fresh fixture lease and focus");
        assert_eq!(
            std::fs::read(&result.path).expect("fixture disk bytes"),
            DOC.as_bytes(),
            "only the exact disposable fixture may be changed"
        );
        result.document = result.send(2357, 0, 0).expect("document identity");
        assert_ne!(result.document, 0);
        assert_eq!(
            result.send(2140, 0, 0).unwrap(),
            0,
            "fixture must begin editable"
        );
        assert_eq!(
            result.send(2570, 0, 0).unwrap(),
            1,
            "fixture must begin with one stream selection"
        );
        assert_eq!(result.send(2423, 0, 0).unwrap(), 0);
        assert_eq!(result.send(2583, 0, 0).unwrap(), 0);
        assert_eq!(result.send(2581, 0, 0).unwrap(), 0);
        result.original_anchor = result.send(2579, 0, 0).unwrap();
        result.original_caret = result.send(2577, 0, 0).unwrap();
        assert_eq!(result.read_document().unwrap(), DOC);
        result.initialized = true;
        result
    }

    /// Re-read the lease before every message, clipboard observation and commit.
    /// The coordinator verifies CIM command-line ownership before issuing the
    /// nonce; this test requires that nonce and the exact canonical fixture path.
    fn guard(&self) -> Result<Duration, String> {
        let bytes = std::fs::read(&self.lease).map_err(|_| "fixture lease unavailable")?;
        if bytes.len() > 32768 {
            return Err("fixture lease oversized".into());
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| "invalid fixture lease")?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "clock before epoch")?
            .as_millis();
        let expiry = value["expiresAtMs"]
            .as_u64()
            .ok_or("lease expiry missing")?;
        if u128::from(expiry) <= now
            || value["targetPid"].as_u64() != Some(u64::from(self.pid))
            || value["ownershipNonce"].as_str() != Some(self.nonce.as_str())
        {
            return Err("fixture lease expired or ownership differs".into());
        }
        let authorized_path = PathBuf::from(
            value["fixturePath"]
                .as_str()
                .ok_or("lease fixture path missing")?,
        )
        .canonicalize()
        .map_err(|_| "lease fixture path unavailable")?;
        if authorized_path != self.path {
            return Err("lease names another fixture".into());
        }
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: query-only HWND checks and initialized GUI out buffer. No
        // foreground activation or input attachment is attempted.
        unsafe {
            let mut exit_code = 0;
            GetExitCodeProcess(self.process.0, &mut exit_code)
                .map_err(|_| "fixture process liveness unavailable")?;
            GetGUIThreadInfo(self.tid, &mut info).map_err(|_| "fixture focus unavailable")?;
            if GetForegroundWindow() != self.main
                || info.hwndFocus != self.editor
                || window_pid_tid(self.main) != (self.pid, self.tid)
                || window_pid_tid(self.editor) != (self.pid, self.tid)
                || GetAncestor(self.editor, GA_ROOT) != self.main
                || class(self.main) != "Notepad++"
                || class(self.editor) != "Scintilla"
                || process_created(self.process.0)? != self.created
                || exit_code != 259
            {
                return Err("fixture identity or foreground/focus changed".into());
            }
            if self.initialized {
                let mut current_document = 0;
                let remaining = u128::from(expiry).saturating_sub(now);
                let timeout =
                    u32::try_from(remaining.min(100)).map_err(|_| "guard timeout overflow")?;
                if timeout == 0
                    || SendMessageTimeoutW(
                        self.editor,
                        2357,
                        WPARAM(0),
                        LPARAM(0),
                        SMTO_ABORTIFHUNG | SMTO_BLOCK,
                        timeout,
                        Some(&mut current_document),
                    )
                    .0 == 0
                    || current_document as isize != self.document
                {
                    return Err("fixture document identity unavailable or changed".into());
                }
            }
        }
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "clock before epoch")?
            .as_millis();
        if after >= u128::from(expiry) {
            return Err("fixture lease exhausted during identity check".into());
        }
        Ok(Duration::from_millis(
            u64::try_from(u128::from(expiry) - after).map_err(|_| "lease duration overflow")?,
        ))
    }

    fn send(&self, message: u32, w: usize, l: isize) -> Result<isize, String> {
        let remaining = self.guard()?;
        let timeout =
            u32::try_from(remaining.as_millis().min(1000)).map_err(|_| "timeout conversion")?;
        if timeout == 0 {
            return Err("fixture lease exhausted".into());
        }
        let mut result = 0usize;
        let start = Instant::now();
        // SAFETY: all probe messages use scalar arguments; no remote writable
        // buffer or target-side pointer into our process is ever supplied.
        let ok = unsafe {
            SendMessageTimeoutW(
                self.editor,
                message,
                WPARAM(w),
                LPARAM(l),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                timeout,
                Some(&mut result),
            )
        };
        if ok.0 == 0 || start.elapsed() >= remaining {
            return Err("fixture scalar timed out".into());
        }
        self.guard()?;
        Ok(result as isize)
    }

    fn read_document(&self) -> Result<String, String> {
        let len = usize::try_from(self.send(2006, 0, 0)?).map_err(|_| "negative document size")?;
        if len > MAX_DOCUMENT {
            return Err("unexpected fixture document size".into());
        }
        if len == 0 {
            return Ok(String::new());
        }
        let address = self.send(2643, 0, len as isize)? as usize;
        if address == 0 || address.checked_add(len).is_none() {
            return Err("invalid fixture read address".into());
        }
        self.guard()?;
        let mut bytes = vec![0u8; len];
        let mut count = 0;
        // SAFETY: a remote address is copied by the kernel, never dereferenced
        // in Rust. Our initialized byte buffer has the exact requested extent.
        unsafe {
            ReadProcessMemory(
                self.process.0,
                address as *const _,
                bytes.as_mut_ptr().cast(),
                len,
                Some(&mut count),
            )
        }
        .map_err(|_| "fixture read failed")?;
        self.guard()?;
        if count != len {
            return Err("partial fixture read".into());
        }
        String::from_utf8(bytes).map_err(|_| "fixture is not UTF-8".into())
    }

    fn select(&self, anchor: usize, caret: usize) {
        self.send(2160, anchor, caret as isize)
            .expect("fixture selection");
    }

    fn digest(&self) -> (u32, Vec<(u32, Option<u64>)>) {
        self.guard().expect("lease before clipboard observation");
        let result = probe::clipboard_digest();
        self.guard().expect("lease after clipboard observation");
        result
    }

    fn capture(&self, session: u64) -> Result<Captured, CaptureError> {
        assert!(self.guard().expect("lease before capture") >= Duration::from_millis(1500));
        let (result, elapsed) = probe::capture(session);
        self.guard().expect("lease after capture");
        eprintln!(
            "scintilla capture session={session} elapsed_ms={}",
            elapsed.as_millis()
        );
        result
    }

    fn validate(&self, captured: &Captured) -> Result<(), super::StaleReason> {
        assert!(self.guard().expect("lease before validation") >= Duration::from_millis(1500));
        let result = super::windows::probe_validate(captured.token.id());
        self.guard().expect("lease after validation");
        result
    }

    fn begin_edit(&mut self) {
        self.send(2078, 0, 0).expect("begin owned undo group");
        self.undo_open = true;
        self.undo_pending = true;
    }

    fn finish_edit(&mut self) {
        if self.undo_open {
            self.send(2079, 0, 0).expect("end owned undo group");
            self.undo_open = false;
        }
    }

    fn undo(&mut self) {
        self.finish_edit();
        self.send(2176, 0, 0).expect("undo only owned edit group");
        assert_eq!(
            self.read_document().unwrap(),
            DOC,
            "owned undo must restore fixture exactly"
        );
        self.undo_pending = false;
    }

    fn deliver(
        &mut self,
        session: u64,
        replacement: &str,
        empty: bool,
    ) -> (Duration, Vec<crate::paste_tx::diag::Event>) {
        let captured = self.capture(session).expect("native production capture");
        assert_eq!(captured.token.is_empty_target(), empty);
        assert!(self.guard().expect("lease before clipboard backup") >= Duration::from_secs(20));
        let mut clipboard = crate::paste_tx::ProbeClipboardSession::begin()
            .expect("lossless user clipboard backup");
        let before = self.digest().1;
        self.begin_edit();
        self.guard()
            .expect("lease immediately before strict commit");
        let authorization = self.authorization();
        let _ = crate::paste_tx::diag::take();
        let started = Instant::now();
        let outcome = super::commit(CommitRequest {
            token: captured.token,
            session: SessionId(session),
            replacement,
            clipboard_handling: crate::settings::ClipboardHandling::DontModify,
            is_cancelled: &|| !authorization.current(),
        });
        let elapsed = started.elapsed();
        let events = crate::paste_tx::diag::take();
        eprintln!("scintilla strict commit session={session} empty={empty} elapsed_ms={} outcome={outcome:?} events={}", elapsed.as_millis(), events.len());
        for (index, event) in events.iter().enumerate() {
            use crate::paste_tx::diag::Event;
            match event {
                Event::FreeCheck { at, free, holder_pid } => eprintln!("scintilla clipboard event={index} at_ms={} free_check free={free} holder_pid={holder_pid}", at.saturating_duration_since(started).as_millis()),
                Event::Injecting { at, holder_pid } => eprintln!("scintilla clipboard event={index} at_ms={} injecting holder_pid={holder_pid}", at.saturating_duration_since(started).as_millis()),
                Event::Render { at, after_injection, rendered, reader_pid } => eprintln!("scintilla clipboard event={index} at_ms={} render after_injection={after_injection} rendered={rendered} reader_pid={reader_pid}", at.saturating_duration_since(started).as_millis()),
            }
        }
        self.guard().expect("lease after strict commit");
        self.finish_edit();
        assert!(
            matches!(
                outcome,
                CommitOutcome::Delivered {
                    clipboard: super::ClipboardSettlement::Restored
                }
            ),
            "strict native delivery: {outcome:?}"
        );
        assert_eq!(
            self.digest().1,
            before,
            "strict commit restores clipboard formats/content"
        );
        let expected = if empty {
            format!("{DOC}{replacement}")
        } else {
            replacement.to_string()
        };
        assert_eq!(self.read_document().unwrap(), expected);
        self.undo();
        self.guard()
            .expect("lease before clipboard session restore");
        let before_cleanup = self.digest();
        let cleanup = clipboard.restore();
        let after_cleanup = self.digest();
        // Strict publication/restoration advances the sequence. The older
        // session backup must refuse to overwrite a sequence it no longer owns.
        assert!(
            matches!(
                cleanup,
                super::ClipboardSettlement::Restored
                    | super::ClipboardSettlement::ExternallyChanged
            ),
            "clipboard session cleanup: {cleanup:?}"
        );
        if cleanup == super::ClipboardSettlement::ExternallyChanged {
            assert_eq!(
                after_cleanup, before_cleanup,
                "stale outer backup must not mutate the clipboard"
            );
        }
        assert_eq!(
            after_cleanup.1, before,
            "session preserves original clipboard"
        );
        (elapsed, events)
    }
}

/// A test-owned clipboard reader: holds the lock only, without enumerating,
/// reading, emptying or publishing any format. The thread closes its own lock
/// before it exits, including if the readiness receiver has disappeared.
fn hold_clipboard_for_retry(
    ready: std::sync::mpsc::Sender<Result<Instant, String>>,
) -> Result<Duration, String> {
    use windows::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};

    struct ClipboardLock;
    impl Drop for ClipboardLock {
        fn drop(&mut self) {
            // SAFETY: this thread owns exactly one successful OpenClipboard.
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    // SAFETY: no HWND is required for a non-writing reader. This thread never
    // calls EmptyClipboard or SetClipboardData, and closes the lock itself.
    if let Err(error) = unsafe { OpenClipboard(None) } {
        let error = format!("owned clipboard holder could not acquire: {error}");
        let _ = ready.send(Err(error.clone()));
        return Err(error);
    }
    let lock = ClipboardLock;
    let acquired = Instant::now();
    if ready.send(Ok(acquired)).is_err() {
        return Err("postpublication readiness receiver disappeared".into());
    }
    // Longer than the old 300ms readiness limit, shorter than the new 1.5s
    // budget. The acquisition handshake prevents scheduler startup from being
    // mistaken for actual clipboard contention.
    std::thread::sleep(Duration::from_millis(500));
    drop(lock);
    Ok(acquired.elapsed())
}

#[test]
#[ignore = "requires coordinator-owned Notepad++ fixture and a fresh foreground lease"]
fn notepad_clipboard_retry() {
    use crate::paste_tx::diag::{Event, AFTER_PUBLISH};
    use std::sync::{Arc, Mutex};

    let mut fixture = Fixture::open();
    fixture.select(0, DOC.len());
    let holder = Arc::new(Mutex::new(None));
    let holder_from_hook = Arc::clone(&holder);
    let authorization = fixture.authorization();
    assert!(AFTER_PUBLISH.lock().unwrap().is_none());
    *AFTER_PUBLISH.lock().unwrap() = Some(Box::new(move || {
        assert!(
            authorization.current(),
            "fresh lease before owned clipboard hold"
        );
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || hold_clipboard_for_retry(ready_tx));
        *holder_from_hook.lock().unwrap() = Some(thread);
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("owned clipboard holder readiness")
            .expect("owned clipboard holder acquisition");
    }));

    // Keep a panic from skipping the join: no test-owned clipboard holder may
    // outlive this probe or its foreground lease.
    let delivery = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fixture.deliver(15_009, "Retry replacement exactly once.", false)
    }));
    let unused_hook = AFTER_PUBLISH.lock().unwrap().take();
    let holder_thread = holder.lock().unwrap().take();
    let held = holder_thread.map(|thread| thread.join().expect("clipboard holder thread"));
    let (elapsed, events) = match delivery {
        Ok(value) => value,
        Err(error) => std::panic::resume_unwind(error),
    };
    assert!(
        unused_hook.is_none(),
        "real strict publication must run the holder hook"
    );
    let held = held
        .expect("postpublication holder was spawned")
        .expect("holder released cleanly");
    assert!(held >= Duration::from_millis(500));
    let busy_checks = events
        .iter()
        .filter(|event| matches!(event, Event::FreeCheck { free: false, .. }))
        .count();
    let injections = events
        .iter()
        .filter(|event| matches!(event, Event::Injecting { .. }))
        .count();
    assert!(
        busy_checks >= 2,
        "must exercise retries against an acquired native lock: {events:?}"
    );
    assert_eq!(injections, 1, "native retry must inject exactly once");
    assert!(elapsed >= Duration::from_millis(500));
    assert_eq!(fixture.read_document().unwrap(), DOC);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), DOC.as_bytes());
    eprintln!("HANDY native postpublication clipboard retry: held_ms={} commit_ms={} busy_checks={busy_checks} injections={injections}; exact replacement, clipboard restoration and document undo passed", held.as_millis(), elapsed.as_millis());
}

struct SuspendedGuiThread(HANDLE);

impl SuspendedGuiThread {
    fn suspend(fixture: &Fixture) -> Self {
        fixture
            .guard()
            .expect("fresh owned fixture lease before suspension");
        // SAFETY: only the already-verified fixture GUI thread is opened. The
        // handle is owned locally; every successful suspension gets one resume.
        let thread = unsafe {
            OpenThread(
                THREAD_QUERY_LIMITED_INFORMATION | THREAD_SUSPEND_RESUME,
                false,
                fixture.tid,
            )
        }
        .expect("owned fixture GUI thread");
        if unsafe { GetProcessIdOfThread(thread) } != fixture.pid
            || !fixture.authorization().current()
        {
            unsafe {
                let _ = CloseHandle(thread);
            }
            panic!("GUI thread ownership changed before suspension");
        }
        let previous = unsafe { SuspendThread(thread) };
        if previous == u32::MAX {
            unsafe {
                let _ = CloseHandle(thread);
            }
            panic!("could not suspend owned fixture GUI thread");
        }
        let suspended = Self(thread);
        assert_eq!(previous, 0, "fixture GUI thread was already suspended");
        suspended
    }
}

impl Drop for SuspendedGuiThread {
    fn drop(&mut self) {
        // SAFETY: this guard owns exactly one successful SuspendThread count.
        // Resume happens before closing the handle, including during unwinding.
        unsafe {
            if ResumeThread(self.0) == u32::MAX {
                eprintln!("owned fixture GUI thread resume failed");
            }
            let _ = CloseHandle(self.0);
        }
    }
}

#[test]
#[ignore = "requires a fresh lease authorizing temporary suspension of the owned Notepad++ GUI thread"]
fn notepad_timeout() {
    let fixture = Fixture::open();
    let before = fixture.digest();
    assert!(fixture.guard().unwrap() >= Duration::from_secs(5));
    let authorization = fixture.authorization();
    let suspended = SuspendedGuiThread::suspend(&fixture);
    // This guard deliberately avoids a target-side message: the owned GUI
    // thread is suspended. Production capture must supply the bounded timeout.
    assert!(
        authorization.current(),
        "fresh lease before hung-target capture"
    );
    let (result, elapsed) = probe::capture(15_100);
    drop(suspended);
    fixture
        .guard()
        .expect("fixture resumed with the same lease/identity");
    assert!(
        matches!(result, Err(CaptureError::Timeout)),
        "hung target must time out: {result:?}"
    );
    assert!(
        elapsed <= super::CAPTURE_TIMEOUT + Duration::from_millis(100),
        "whole capture timeout exceeded caller grace: {elapsed:?}"
    );
    assert_eq!(
        fixture.digest(),
        before,
        "timeout leaves clipboard untouched"
    );
    assert_eq!(
        fixture.read_document().unwrap(),
        DOC,
        "timeout leaves fixture unchanged"
    );
    eprintln!(
        "HANDY-015 suspended-owned-GUI capture timeout elapsed_ms={}",
        elapsed.as_millis()
    );
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Cleanup never reacquires foreground or broadens the target. If the
        // lease/focus was lost, leave the owned fixture for the coordinator.
        if self.initialized && self.guard().is_ok() {
            let clean = (|| -> Result<(), String> {
                if self.send(2357, 0, 0)? != self.document {
                    return Err("fixture document changed; cleanup refused".into());
                }
                self.send(2171, 0, 0)?;
                self.send(2571, 0, 0)?;
                if self.undo_open {
                    self.send(2079, 0, 0)?;
                }
                if self.undo_pending && self.read_document()? != DOC {
                    self.send(2176, 0, 0)?;
                }
                if self.read_document()? != DOC {
                    return Err("fixture remains changed; no repair attempted".into());
                }
                self.send(2160, self.original_anchor as usize, self.original_caret)?;
                Ok(())
            })();
            if let Err(error) = clean {
                eprintln!("owned fixture cleanup: {error}");
            }
        }
    }
}

#[test]
#[ignore = "requires coordinator-owned Notepad++ fixture and a fresh foreground lease"]
fn notepad_live() {
    let mut fixture = Fixture::open();
    fixture.select(0, DOC.len());
    let before = fixture.digest();
    let selected = fixture
        .capture(15_001)
        .expect("production dispatch must support the gated native fixture");
    assert_eq!(selected.text, CapturedText::Selected(DOC.to_string()));
    assert_eq!(
        fixture.digest(),
        before,
        "capture must not read or modify clipboard"
    );
    fixture.validate(&selected).expect("unchanged native token");
    fixture.send(2578, 0, 1).unwrap();
    fixture.send(2578, 0, 0).unwrap();
    assert!(
        fixture.validate(&selected).is_err(),
        "anchor-only ABA must revoke authority"
    );
    drop(selected);

    let selected = fixture.capture(15_002).unwrap();
    fixture
        .validate(&selected)
        .expect("stable before caret ABA");
    fixture.select(0, DOC.len() - 1);
    fixture.select(0, DOC.len());
    assert!(
        fixture.validate(&selected).is_err(),
        "caret move-and-return must revoke authority"
    );
    drop(selected);

    fixture.select(1, 1);
    let caret = fixture.capture(15_003).unwrap();
    assert_eq!(caret.text, CapturedText::Empty);
    fixture.validate(&caret).expect("stable before edit ABA");
    fixture.begin_edit();
    fixture.send(2326, 0, 0).unwrap();
    fixture.undo();
    fixture.select(1, 1);
    assert!(
        fixture.validate(&caret).is_err(),
        "edit-and-undo must revoke authority"
    );
    drop(caret);

    let before_refusals = fixture.digest();
    fixture.send(2171, 1, 0).unwrap();
    assert!(matches!(
        fixture.capture(15_004),
        Err(CaptureError::ReadOnly)
    ));
    fixture.send(2171, 0, 0).unwrap();
    fixture.select(0, 0);
    fixture.send(2573, 1, 1).unwrap();
    assert!(matches!(
        fixture.capture(15_005),
        Err(CaptureError::MultipleSelections)
    ));
    fixture.send(2571, 0, 0).unwrap();
    assert_eq!(
        fixture.digest(),
        before_refusals,
        "refusals leave clipboard untouched"
    );
    assert_eq!(fixture.read_document().unwrap(), DOC);

    fixture.select(0, DOC.len());
    fixture.deliver(15_006, "Replacement zażółć.\r\nSecond ✅.", false);
    fixture.select(DOC.len(), DOC.len());
    fixture.deliver(15_007, " Inserted 😀.", true);
    assert_eq!(fixture.read_document().unwrap(), DOC);
    assert_eq!(
        std::fs::read(&fixture.path).unwrap(),
        DOC.as_bytes(),
        "probe never saves file changes"
    );
    eprintln!("HANDY-015 production probe: selected/empty; anchor/caret/edit ABA; readonly/multiselection refusal; strict replacement/insertion; exact document and clipboard restoration passed");
}

#[test]
#[ignore = "requires coordinator-owned Notepad++ fixture and a fresh foreground lease"]
fn notepad_insert_only() {
    let mut fixture = Fixture::open();
    fixture.select(DOC.len(), DOC.len());
    fixture.deliver(15_008, " Inserted 😀.", true);
    assert_eq!(fixture.read_document().unwrap(), DOC);
    assert_eq!(
        std::fs::read(&fixture.path).unwrap(),
        DOC.as_bytes(),
        "probe never saves file changes"
    );
    eprintln!("HANDY-015 isolated empty-target strict insertion and document restoration passed");
}
