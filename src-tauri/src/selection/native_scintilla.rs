//! Read-only native transport. No pointer supplied to Scintilla addresses our
//! memory; returned remote addresses are copied only by ReadProcessMemory.

use std::time::Instant;
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::SystemInformation::{
    IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_UNKNOWN,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, IsWow64Process2, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetAncestor, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
    IsWindowVisible, RealGetWindowClassW, SendMessageTimeoutW, GA_ROOT, GUITHREADINFO,
    SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_PAINT,
};

use super::native_scintilla_evidence::{self as model, Error, Metadata};

/// The exact admitted release passed the native notification/barrier gate.
/// See HANDY-015's design and retained native receipts before broadening it.
pub(super) const ENABLED: bool = true;

struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: this wrapper exclusively owns one successful OpenProcess handle.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub(super) struct Target {
    process: Process,
    pub foreground: HWND,
    pub editor: HWND,
    status: HWND,
    pid: u32,
    tid: u32,
    created: u64,
    pub original: Metadata,
    text: String,
}

fn deadline_check(deadline: Instant) -> Result<(), Error> {
    if Instant::now() >= deadline {
        Err(Error::Timeout)
    } else {
        Ok(())
    }
}

fn scalar(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
    deadline: Instant,
) -> Result<isize, Error> {
    deadline_check(deadline)?;
    let millis = deadline
        .saturating_duration_since(Instant::now())
        .as_millis();
    if millis == 0 {
        return Err(Error::Timeout);
    }
    let timeout = u32::try_from(millis).unwrap_or(u32::MAX);
    let mut result = 0usize;
    // SAFETY: only integer scalar arguments are sent; result points to a local
    // initialized usize used synchronously by user32, never by the remote editor.
    let sent = unsafe {
        SendMessageTimeoutW(
            hwnd,
            message,
            WPARAM(wparam),
            LPARAM(lparam),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            timeout,
            Some(&mut result),
        )
    };
    model::scalar_result(sent.0 != 0, Instant::now() < deadline, result)
}

fn pid_tid(hwnd: HWND) -> (u32, u32) {
    let mut pid = 0;
    // SAFETY: local out parameter; no handle ownership is transferred.
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid, tid)
}

fn class(hwnd: HWND) -> String {
    let mut bytes = [0u16; 256];
    // SAFETY: user32 receives the correctly sized local UTF-16 slice.
    let count = unsafe { RealGetWindowClassW(hwnd, &mut bytes) } as usize;
    String::from_utf16_lossy(&bytes[..count.min(bytes.len())])
}

fn focus(tid: u32) -> Result<HWND, Error> {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: properly sized initialized GUI-thread information out buffer.
    unsafe { GetGUIThreadInfo(tid, &mut info) }.map_err(|_| Error::Identity)?;
    if info.hwndFocus.is_invalid() {
        return Err(Error::Identity);
    }
    Ok(info.hwndFocus)
}

fn created(process: HANDLE) -> Result<u64, Error> {
    let (mut birth, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: retained query handle and four disjoint local out parameters.
    unsafe { GetProcessTimes(process, &mut birth, &mut exit, &mut kernel, &mut user) }
        .map_err(|_| Error::Identity)?;
    Ok((u64::from(birth.dwHighDateTime) << 32) | u64::from(birth.dwLowDateTime))
}

fn tested_executable(process: HANDLE, deadline: Instant) -> Result<(), Error> {
    if !cfg!(target_arch = "x86_64") {
        return Err(Error::Unsupported);
    }
    let mut process_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut path = vec![0u16; 32768];
    let mut count = path.len() as u32;
    // SAFETY: retained query handle; initialized local out buffers and bounds.
    unsafe {
        IsWow64Process2(process, &mut process_machine, Some(&mut native_machine))
            .map_err(|_| Error::Identity)?;
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut count,
        )
        .map_err(|_| Error::Identity)?;
    }
    if process_machine != IMAGE_FILE_MACHINE_UNKNOWN
        || native_machine != IMAGE_FILE_MACHINE_AMD64
        || count as usize >= path.len()
    {
        return Err(Error::Unsupported);
    }
    path.truncate(count as usize);
    let name = String::from_utf16(&path).map_err(|_| Error::Identity)?;
    if !name
        .rsplit(['\\', '/'])
        .next()
        .is_some_and(|s| s.eq_ignore_ascii_case("notepad++.exe"))
    {
        return Err(Error::Unsupported);
    }
    path.push(0);
    // SAFETY: NUL-terminated local filename, bounded version-data allocation;
    // VerQueryValue returns a pointer into that live allocation. Copy unaligned
    // only after verifying its complete byte extent lies within the allocation.
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(path.as_ptr()), None);
        if size == 0 || size > 1_048_576 {
            return Err(Error::Unsupported);
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(path.as_ptr()), None, size, data.as_mut_ptr().cast())
            .map_err(|_| Error::Identity)?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0;
        if !VerQueryValueW(data.as_ptr().cast(), w!("\\"), &mut ptr, &mut len).as_bool() {
            return Err(Error::Identity);
        }
        let first = data.as_ptr() as usize;
        let address = ptr as usize;
        let needed = std::mem::size_of::<VS_FIXEDFILEINFO>();
        if len as usize != needed
            || address < first
            || address
                .checked_add(needed)
                .is_none_or(|end| end > first + data.len())
        {
            return Err(Error::Identity);
        }
        let info = std::ptr::read_unaligned(ptr.cast::<VS_FIXEDFILEINFO>());
        if info.dwSignature != 0xFEEF04BD
            || !model::tested_version(info.dwFileVersionMS, info.dwFileVersionLS)
        {
            return Err(Error::Unsupported);
        }
    }
    deadline_check(deadline)
}

impl Target {
    /// Recognition happens before native dispatch; unrelated windows retain UIA.
    pub(super) fn recognizes(foreground: HWND) -> bool {
        class(foreground) == "Notepad++"
    }

    pub(super) fn open(foreground: HWND, deadline: Instant) -> Result<Self, Error> {
        deadline_check(deadline)?;
        let (pid, tid) = pid_tid(foreground);
        if pid == 0 || tid == 0 || !Self::recognizes(foreground) {
            return Err(Error::Unsupported);
        }
        let editor = focus(tid)?;
        // SAFETY: query-only window checks; no borrowed or owned pointer values.
        unsafe {
            if class(editor) != "Scintilla"
                || pid_tid(editor) != (pid, tid)
                || GetAncestor(editor, GA_ROOT) != foreground
            {
                return Err(Error::Unsupported);
            }
            let process = Process(
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                    false,
                    pid,
                )
                .map_err(|_| Error::Identity)?,
            );
            tested_executable(process.0, deadline)?;
            let status = FindWindowExW(Some(foreground), None, w!("msctls_statusbar32"), None)
                .map_err(|_| Error::Unsupported)?;
            let target = Self {
                created: created(process.0)?,
                process,
                foreground,
                editor,
                status,
                pid,
                tid,
                original: Metadata {
                    document: 0,
                    length: 0,
                    codepage: 0,
                    read_only: false,
                    selections: 0,
                    mode: 0,
                    main: 0,
                    anchor: 0,
                    caret: 0,
                    anchor_virtual: 0,
                    caret_virtual: 0,
                    start: 0,
                    end: 0,
                },
                text: String::new(),
            };
            target.identity(deadline)?;
            Ok(target)
        }
    }

    pub(super) fn identity(&self, deadline: Instant) -> Result<(), Error> {
        deadline_check(deadline)?;
        let mut exit = 0;
        // SAFETY: retained query handle and local out parameter; HWND calls are
        // observations and the original process/thread/root are checked afresh.
        unsafe {
            GetExitCodeProcess(self.process.0, &mut exit).map_err(|_| Error::Identity)?;
            if exit != 259
                || created(self.process.0)? != self.created
                || GetForegroundWindow() != self.foreground
                || pid_tid(self.foreground) != (self.pid, self.tid)
                || pid_tid(self.editor) != (self.pid, self.tid)
                || focus(self.tid)? != self.editor
                || GetAncestor(self.editor, GA_ROOT) != self.foreground
                || class(self.foreground) != "Notepad++"
                || class(self.editor) != "Scintilla"
            {
                return Err(Error::Identity);
            }
            if !IsWindowEnabled(self.editor).as_bool() {
                return Err(Error::Disabled);
            }
            if !IsWindowVisible(self.editor).as_bool()
                || !IsWindowVisible(self.status).as_bool()
                || !IsWindowEnabled(self.status).as_bool()
                || pid_tid(self.status).0 != self.pid
                || GetAncestor(self.status, GA_ROOT) != self.foreground
                || class(self.status) != "msctls_statusbar32"
            {
                return Err(Error::Unsupported);
            }
        }
        deadline_check(deadline)
    }

    fn metadata(&self, deadline: Instant) -> Result<Metadata, Error> {
        let q = |message, w| scalar(self.editor, message, w, 0, deadline);
        let main = q(2575, 0)?;
        if main != 0 {
            return Err(Error::MultipleSelections);
        }
        let m = Metadata {
            document: q(2357, 0)? as usize,
            length: q(2006, 0)?,
            codepage: q(2137, 0)?,
            read_only: q(2140, 0)? != 0,
            selections: q(2570, 0)?,
            mode: q(2423, 0)?,
            main,
            anchor: q(2579, 0)?,
            caret: q(2577, 0)?,
            anchor_virtual: q(2583, 0)?,
            caret_virtual: q(2581, 0)?,
            start: q(2143, 0)?,
            end: q(2145, 0)?,
        };
        m.range()?;
        Ok(m)
    }

    fn read(&self, start: usize, count: usize, deadline: Instant) -> Result<String, Error> {
        if count > model::MAX_BYTES {
            return Err(Error::TooLarge);
        }
        if count == 0 {
            deadline_check(deadline)?;
            return Ok(String::new());
        }
        let address = scalar(
            self.editor,
            2643,
            start,
            isize::try_from(count).map_err(|_| Error::TooLarge)?,
            deadline,
        )? as usize;
        if address == 0 || address.checked_add(count).is_none() {
            return Err(Error::ReadFailed);
        }
        let mut bytes = vec![0u8; count];
        let mut read = 0;
        // SAFETY: remote address is never dereferenced by Rust. Kernel copies at
        // most count bytes into our initialized, count-sized allocation. A remote
        // free/race may fail or copy partial data, both refused. No remote buffers
        // are allocated/freed, so timeout cannot leave a remote writable pointer.
        unsafe {
            ReadProcessMemory(
                self.process.0,
                address as *const _,
                bytes.as_mut_ptr().cast(),
                count,
                Some(&mut read),
            )
        }
        .map_err(|_| Error::ReadFailed)?;
        deadline_check(deadline)?;
        if read != count {
            return Err(Error::ReadFailed);
        }
        model::decode(bytes, count)
    }

    pub(super) fn barrier(&self, deadline: Instant) -> Result<(), Error> {
        self.identity(deadline)?;
        // SAFETY: asynchronous invalidation supplies no remote pointers; the
        // target-side WM_PAINT flush is separately timeout-bounded below.
        if !unsafe { InvalidateRect(Some(self.editor), None, false) }.as_bool() {
            return Err(Error::ReadFailed);
        }
        scalar(self.editor, WM_PAINT, 0, 0, deadline)?;
        Ok(())
    }

    fn snapshot(&self, deadline: Instant) -> Result<(Metadata, String), Error> {
        self.identity(deadline)?;
        let before = self.metadata(deadline)?;
        let (start, count) = before.range()?;
        let first = self.read(start, count, deadline)?;
        let second = self.read(start, count, deadline)?;
        let after = self.metadata(deadline)?;
        self.identity(deadline)?;
        if before != after || first != second {
            return Err(Error::Changed);
        }
        Ok((after, first))
    }

    pub(super) fn capture(&mut self, deadline: Instant) -> Result<Option<String>, Error> {
        let (metadata, text) = self.snapshot(deadline)?;
        let empty = metadata.anchor == metadata.caret;
        self.original = metadata;
        self.text = text;
        Ok(if empty { None } else { Some(self.text.clone()) })
    }

    pub(super) fn validate(&self, deadline: Instant) -> Result<(), Error> {
        let (metadata, text) = self.snapshot(deadline)?;
        model::compare(&self.original, &self.text, &metadata, &text)
    }

    pub(super) fn verify(&self, replacement: &str, deadline: Instant) -> bool {
        let check = || -> Result<bool, Error> {
            self.identity(deadline)?;
            let before = self.metadata(deadline)?;
            if before.anchor != before.caret || before.caret <= self.original.start {
                return Ok(false);
            }
            let count = usize::try_from(before.caret - self.original.start)
                .map_err(|_| Error::InvalidPosition)?;
            let start = usize::try_from(self.original.start).map_err(|_| Error::InvalidPosition)?;
            let text = self.read(start, count, deadline)?;
            let second = self.read(start, count, deadline)?;
            let after = self.metadata(deadline)?;
            self.identity(deadline)?;
            Ok(before == after
                && text == second
                && model::verify(&self.original, &after, &text, replacement))
        };
        check().unwrap_or(false)
    }
}
