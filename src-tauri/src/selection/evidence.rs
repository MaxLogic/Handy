//! Pure, platform-neutral facts and decisions for capture, invalidation,
//! validation and post-paste verification. The Windows worker gathers facts;
//! everything that decides lives here so it is unit-tested.

use super::{CaptureError, CapturedText, InvalidationReason, StaleReason, MAX_SELECTION_SCALARS};

/// Completes validation while the invalidation hooks are still installed.
pub(crate) fn finish_validation(
    result: Result<(), StaleReason>,
    boundary: bool,
    flag: &std::sync::atomic::AtomicU8,
    drain: impl FnOnce(),
    disarm: impl FnOnce(),
) -> Result<(), StaleReason> {
    // UIA reads do not pump out-of-context WinEvents. Changes (including
    // change-and-change-back) queued during those reads still revoke authority.
    drain();
    if let Some(reason) = super::invalidation_of(flag) {
        return Err(StaleReason::Invalidated(reason));
    }
    if result.is_ok() && boundary {
        disarm();
    }
    result
}

/// Verifies text immediately before the caret against capture-time position.
pub(crate) fn verify_insertion(
    empty_target: bool,
    caret_from_original_start: i32,
    observed: &str,
    replacement: &str,
) -> bool {
    (!empty_target || caret_from_original_start > 0) && inserted_text_matches(observed, replacement)
}

/// UIA control type id of an edit control (UIA_EditControlTypeId).
pub(crate) const UIA_EDIT_CONTROL_TYPE: i32 = 50004;

/// Win32 classes (by `RealGetWindowClass`, case-insensitive) whose UIA
/// providers expose the TextPattern evidence this contract needs.
pub(crate) const WIN32_EDIT_CLASSES: &[&str] =
    &["Edit", "RichEdit20W", "RICHEDIT50W", "RichEditD2DPT"];

/// One allowlisted provider family. Extend only with probe evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SupportedProvider {
    /// FrameworkId `Win32` with a real window class in `WIN32_EDIT_CLASSES`.
    Win32Edit,
    /// FrameworkId `Chrome`, an Edit control that is not a Monaco/VS Code
    /// `inputarea` (which mirrors only part of the document).
    ChromiumEdit,
}

pub(crate) const SUPPORTED_PROVIDERS: &[SupportedProvider] = &[
    SupportedProvider::Win32Edit,
    SupportedProvider::ChromiumEdit,
];

/// Matches the focused element against the allowlist.
pub(crate) fn supported_provider(
    framework_id: &str,
    control_type: i32,
    class_name: &str,
    real_class: Option<&str>,
) -> Option<SupportedProvider> {
    SUPPORTED_PROVIDERS.iter().copied().find(|p| match p {
        SupportedProvider::Win32Edit => {
            framework_id.eq_ignore_ascii_case("Win32")
                && real_class.is_some_and(|rc| {
                    WIN32_EDIT_CLASSES
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(rc))
                })
        }
        SupportedProvider::ChromiumEdit => {
            framework_id.eq_ignore_ascii_case("Chrome")
                && control_type == UIA_EDIT_CONTROL_TYPE
                && !class_name.to_ascii_lowercase().contains("inputarea")
        }
    })
}

/// Everything capture observed about the focused element. Fields after a
/// failed prerequisite keep their defaults; `classify_capture` stops first.
#[derive(Debug, Clone, Default)]
pub(crate) struct CaptureFacts {
    pub own_pid: u32,
    pub foreground_pid: u32,
    pub focused_pid: u32,
    pub framework_id: String,
    pub control_type: i32,
    pub class_name: String,
    /// `RealGetWindowClass` of the element's native window, Win32 only.
    pub real_class: Option<String>,
    pub is_password: bool,
    pub is_enabled: bool,
    pub has_text_pattern: bool,
    /// ValuePattern.IsReadOnly; `None` when ValuePattern is unavailable.
    pub value_read_only: Option<bool>,
    /// UIA_IsReadOnlyAttributeId of the selection; `None` unless VT_BOOL.
    pub text_attr_read_only: Option<bool>,
    /// `ES_READONLY` of a Win32 edit's style.
    pub win32_read_only: Option<bool>,
    /// Whether `FindAttribute(IsReadOnly = true)` found read-only text
    /// inside a non-degenerate selection; `None` if the provider could not
    /// answer. Catches interior read-only regions that endpoint samples miss.
    pub interior_read_only: Option<bool>,
    pub range_count: i32,
    pub degenerate: bool,
    /// Selected text as read (UTF-16), for non-degenerate ranges.
    pub text: Option<Vec<u16>>,
}

/// Combines the read-only text attribute sampled at both selection
/// endpoints: any `true` wins, then any `false`, else unknown.
pub(crate) fn combine_read_only(start: Option<bool>, end: Option<bool>) -> Option<bool> {
    match (start, end) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), _) | (_, Some(false)) => Some(false),
        _ => None,
    }
}

/// Decides the capture result from the gathered facts.
pub(crate) fn classify_capture(f: &CaptureFacts) -> Result<CapturedText, CaptureError> {
    if f.foreground_pid == f.own_pid {
        return Err(CaptureError::HandyWindowFocused);
    }
    if f.focused_pid != f.foreground_pid {
        return Err(CaptureError::FocusOutsideForegroundApp);
    }
    if supported_provider(
        &f.framework_id,
        f.control_type,
        &f.class_name,
        f.real_class.as_deref(),
    )
    .is_none()
    {
        return Err(CaptureError::UnsupportedTarget);
    }
    if !f.is_enabled {
        return Err(CaptureError::Disabled);
    }
    if f.is_password {
        return Err(CaptureError::PasswordField);
    }
    if !f.has_text_pattern {
        return Err(CaptureError::NoTextPattern);
    }
    match f.range_count {
        0 => return Err(CaptureError::NoCaret),
        1 => {}
        _ => return Err(CaptureError::MultipleSelections),
    }
    // Element-level evidence (ValuePattern.IsReadOnly, ES_READONLY) refuses
    // regardless of what the selection's text attributes say. The text
    // attribute is sampled at both endpoints (a whole-range read is
    // superlinear in Chromium) and the selection is scanned with
    // FindAttribute for any run a provider reports read-only, so an interior
    // read-only region refuses even when both endpoints are editable.
    // Probe evidence (Edge): a contenteditable root (UIA Edit) with a
    // contenteditable=false island reports the island's text as
    // IsReadOnly=false and the scan finds nothing, because the editing host
    // can delete such an island as a unit; UIA gives no signal for it, so
    // such a selection is captured with the island's text included. This is
    // an accepted Chromium semantic (coordinator decision, HANDY-002): the
    // user selected the island and the editor deletes it as a unit. Win32
    // Edit/RichEdit have no per-run read-only state.
    let signals = [f.value_read_only, f.text_attr_read_only, f.win32_read_only];
    if signals.contains(&Some(true)) || f.interior_read_only == Some(true) {
        return Err(CaptureError::ReadOnly);
    }
    if !signals.contains(&Some(false)) {
        return Err(CaptureError::EditabilityUnknown);
    }
    if f.degenerate {
        return Ok(CapturedText::Empty);
    }
    let units = f.text.as_deref().unwrap_or_default();
    if units.len() > 2 * MAX_SELECTION_SCALARS {
        return Err(CaptureError::SelectionTooLarge);
    }
    let text = String::from_utf16(units).map_err(|_| CaptureError::InvalidText)?;
    if text.chars().count() > MAX_SELECTION_SCALARS {
        return Err(CaptureError::SelectionTooLarge);
    }
    if text.is_empty() {
        return Err(CaptureError::EmptyNonDegenerate);
    }
    if text.contains('\u{FFFC}') {
        return Err(CaptureError::EmbeddedObject);
    }
    Ok(CapturedText::Selected(text))
}

// WinEvent ids (winuser.h), numeric so classification is testable anywhere.
pub(crate) const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
pub(crate) const EVENT_OBJECT_FOCUS: u32 = 0x8005;
pub(crate) const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
#[cfg(test)]
pub(crate) const EVENT_OBJECT_VALUECHANGE: u32 = 0x800E;
pub(crate) const EVENT_OBJECT_TEXTSELECTIONCHANGED: u32 = 0x8014;
pub(crate) const OBJID_CARET: i32 = -8;

/// Maps a WinEvent to the invalidation it causes, if any.
pub(crate) fn classify_event(event: u32, id_object: i32) -> Option<InvalidationReason> {
    match event {
        EVENT_SYSTEM_FOREGROUND => Some(InvalidationReason::Foreground),
        EVENT_OBJECT_FOCUS => Some(InvalidationReason::Focus),
        EVENT_OBJECT_LOCATIONCHANGE if id_object == OBJID_CARET => Some(InvalidationReason::Caret),
        EVENT_OBJECT_TEXTSELECTIONCHANGED => Some(InvalidationReason::TextSelection),
        _ => None,
    }
}

/// What capture recorded about the selection itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OriginalSelection {
    Selected(Vec<u16>),
    Empty,
}

/// Identity evidence retained by the worker for a captured target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetIdentity {
    pub foreground_hwnd: usize,
    pub pid: u32,
    pub thread_id: u32,
    /// Process creation time (FILETIME as u64): guards against PID reuse.
    pub process_created: u64,
    /// `GetGUIThreadInfo().hwndFocus` at capture; 0 if unavailable.
    pub gui_focus_hwnd: usize,
    pub runtime_id: Vec<i32>,
    pub focused_pid: u32,
    pub original: OriginalSelection,
}

/// Fresh reads of the live target, in the order `compare_evidence` needs
/// them. Implementations may fail any read with a `StaleReason`.
pub(crate) trait LiveTarget {
    fn foreground_hwnd(&mut self) -> usize;
    /// (pid, process creation time) of `hwnd`'s process.
    fn process_identity(&mut self, hwnd: usize) -> Result<(u32, u64), StaleReason>;
    fn gui_focus_hwnd(&mut self, thread_id: u32) -> usize;
    /// (RuntimeId, ProcessId) of the currently focused element.
    fn focused_element(&mut self) -> Result<(Vec<i32>, u32), StaleReason>;
    fn selection_count(&mut self) -> Result<i32, StaleReason>;
    /// `CompareEndpoints` of the fresh selection against the stored range:
    /// (Start vs Start, End vs End).
    fn endpoint_deltas(&mut self) -> Result<(i32, i32), StaleReason>;
    fn selection_text(&mut self, max_len: i32) -> Result<Vec<u16>, StaleReason>;
    fn selection_degenerate(&mut self) -> Result<bool, StaleReason>;
}

/// Compares live facts with the captured evidence, stopping at the first
/// mismatch, cheapest checks first.
pub(crate) fn compare_evidence(
    identity: &TargetIdentity,
    invalidation: Option<InvalidationReason>,
    live: &mut dyn LiveTarget,
) -> Result<(), StaleReason> {
    if let Some(reason) = invalidation {
        return Err(StaleReason::Invalidated(reason));
    }
    if live.foreground_hwnd() != identity.foreground_hwnd {
        return Err(StaleReason::ForegroundChanged);
    }
    let (pid, created) = live.process_identity(identity.foreground_hwnd)?;
    if pid != identity.pid || created != identity.process_created {
        return Err(StaleReason::ProcessChanged);
    }
    if identity.gui_focus_hwnd != 0
        && live.gui_focus_hwnd(identity.thread_id) != identity.gui_focus_hwnd
    {
        return Err(StaleReason::FocusChanged);
    }
    let (runtime_id, focused_pid) = live.focused_element()?;
    if runtime_id != identity.runtime_id || focused_pid != identity.focused_pid {
        return Err(StaleReason::FocusChanged);
    }
    if live.selection_count()? != 1 {
        return Err(StaleReason::SelectionChanged);
    }
    if live.endpoint_deltas()? != (0, 0) {
        return Err(StaleReason::SelectionChanged);
    }
    match &identity.original {
        OriginalSelection::Selected(original) => {
            let max = i32::try_from(original.len() + 1).unwrap_or(i32::MAX);
            if live.selection_text(max)? != *original {
                return Err(StaleReason::TextChanged);
            }
        }
        OriginalSelection::Empty => {
            if !live.selection_degenerate()? {
                return Err(StaleReason::NotCollapsed);
            }
        }
    }
    Ok(())
}

fn normalize_for_compare(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\u{00A0}', " ")
}

/// Whether the text before the caret ends with the replacement, after
/// normalising line endings (editors store CRLF, CR or LF) and NBSP.
pub(crate) fn inserted_text_matches(read_before_caret: &str, replacement: &str) -> bool {
    let replacement = normalize_for_compare(replacement);
    !replacement.is_empty() && normalize_for_compare(read_before_caret).ends_with(&replacement)
}

/// Characters to move back from the caret to cover the replacement: UTF-16
/// length plus one per line break (an LF may become CRLF) plus one.
pub(crate) fn verify_span(replacement: &str) -> i32 {
    let units = replacement.encode_utf16().count();
    let newlines = replacement.matches('\n').count();
    i32::try_from(units + newlines + 1).unwrap_or(i32::MAX)
}
