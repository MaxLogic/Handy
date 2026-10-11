//! `selection::tests`: pure capture/event/validation decisions, the capture
//! waiter, and the strict commit sequence against a fake platform.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::commit::{
    run_commit, BeginClipboardError, CommitPlatform, CLIPBOARD_FREE_WAIT, MODIFIER_WAIT,
};
use super::evidence::*;
use super::*;
use crate::paste_tx::{ChordResult, LossyReason, StrictSettlement};

// --- helpers -----------------------------------------------------------------

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Facts of a healthy, editable Win32 Edit with a non-empty selection.
fn edit_facts(selected: &str) -> CaptureFacts {
    CaptureFacts {
        own_pid: 1,
        foreground_pid: 100,
        focused_pid: 100,
        framework_id: "Win32".into(),
        control_type: UIA_EDIT_CONTROL_TYPE,
        class_name: "Edit".into(),
        real_class: Some("Edit".into()),
        is_password: false,
        is_enabled: true,
        has_text_pattern: true,
        value_read_only: Some(false),
        text_attr_read_only: None,
        win32_read_only: Some(false),
        interior_read_only: Some(false),
        range_count: 1,
        degenerate: selected.is_empty(),
        text: (!selected.is_empty()).then(|| utf16(selected)),
    }
}

fn chrome_facts(selected: &str) -> CaptureFacts {
    CaptureFacts {
        framework_id: "Chrome".into(),
        class_name: String::new(),
        real_class: None,
        win32_read_only: None,
        ..edit_facts(selected)
    }
}

/// A token whose release is observable on the returned channel.
fn token(id: u64, session: u64) -> (TargetToken, Receiver<TargetId>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let releaser: Arc<dyn Fn(TargetId) + Send + Sync> = Arc::new(move |id| {
        let _ = tx.lock().unwrap().send(id);
    });
    (
        TargetToken::new(
            TargetId(id),
            SessionId(session),
            false,
            Arc::new(AtomicU8::new(0)),
            releaser,
        ),
        rx,
    )
}

// --- capture classification ------------------------------------------------

#[test]
fn capture_selected_preserves_multiline_and_edge_whitespace() {
    let text = "  first line\r\n\tsecond line \n";
    assert_eq!(
        classify_capture(&edit_facts(text)),
        Ok(CapturedText::Selected(text.to_string()))
    );
    assert_eq!(
        classify_capture(&chrome_facts("a\nb ")),
        Ok(CapturedText::Selected("a\nb ".to_string()))
    );
}

#[test]
fn capture_whitespace_only_is_selected() {
    assert_eq!(
        classify_capture(&edit_facts(" \t\r\n ")),
        Ok(CapturedText::Selected(" \t\r\n ".to_string()))
    );
}

#[test]
fn capture_collapsed_range_is_empty() {
    assert_eq!(classify_capture(&edit_facts("")), Ok(CapturedText::Empty));
}

#[test]
fn capture_no_range_is_no_caret() {
    let mut f = edit_facts("");
    f.range_count = 0;
    assert_eq!(classify_capture(&f), Err(CaptureError::NoCaret));
}

#[test]
fn capture_multiple_ranges_is_error() {
    let mut f = edit_facts("x");
    f.range_count = 2;
    assert_eq!(classify_capture(&f), Err(CaptureError::MultipleSelections));
}

#[test]
fn capture_password_is_error() {
    let mut f = edit_facts("secret");
    f.is_password = true;
    assert_eq!(classify_capture(&f), Err(CaptureError::PasswordField));
    let mut f = edit_facts("x");
    f.is_enabled = false;
    assert_eq!(classify_capture(&f), Err(CaptureError::Disabled));
}

#[test]
fn capture_read_only_signal_wins() {
    // Any read-only signal refuses, even when another says editable.
    for set in [
        |f: &mut CaptureFacts| f.value_read_only = Some(true),
        |f: &mut CaptureFacts| f.text_attr_read_only = Some(true),
        |f: &mut CaptureFacts| f.win32_read_only = Some(true),
    ] {
        let mut f = edit_facts("x");
        set(&mut f);
        assert_eq!(classify_capture(&f), Err(CaptureError::ReadOnly));
    }
}

#[test]
fn capture_element_read_only_wins_over_editable_endpoints() {
    // The element reports read-only while both endpoint samples and the
    // interior scan say editable: refuse.
    let mut f = edit_facts("x");
    f.value_read_only = Some(true);
    f.text_attr_read_only = Some(false);
    f.interior_read_only = Some(false);
    assert_eq!(classify_capture(&f), Err(CaptureError::ReadOnly));
    let mut f = chrome_facts("x");
    f.value_read_only = Some(true);
    f.text_attr_read_only = Some(false);
    assert_eq!(classify_capture(&f), Err(CaptureError::ReadOnly));
}

#[test]
fn capture_interior_read_only_region_is_refused() {
    // Editable endpoints and element, read-only island in between.
    let mut f = chrome_facts("start LOCKED end");
    f.value_read_only = Some(false);
    f.text_attr_read_only = Some(false);
    f.interior_read_only = Some(true);
    assert_eq!(classify_capture(&f), Err(CaptureError::ReadOnly));
    // An unanswerable scan is not read-only evidence by itself.
    f.interior_read_only = None;
    assert!(classify_capture(&f).is_ok());
}

#[test]
fn capture_endpoint_read_only_samples_combine() {
    assert_eq!(combine_read_only(Some(false), Some(true)), Some(true));
    assert_eq!(combine_read_only(Some(true), None), Some(true));
    assert_eq!(combine_read_only(Some(false), None), Some(false));
    assert_eq!(combine_read_only(None, Some(false)), Some(false));
    assert_eq!(combine_read_only(None, None), None);
}

#[test]
fn capture_unknown_editability_is_error() {
    let mut f = edit_facts("x");
    f.value_read_only = None;
    f.text_attr_read_only = None;
    f.win32_read_only = None;
    assert_eq!(classify_capture(&f), Err(CaptureError::EditabilityUnknown));
    // One positive editable signal suffices.
    f.text_attr_read_only = Some(false);
    assert!(classify_capture(&f).is_ok());
}

#[test]
fn capture_no_text_pattern_is_error() {
    let mut f = edit_facts("x");
    f.has_text_pattern = false;
    assert_eq!(classify_capture(&f), Err(CaptureError::NoTextPattern));
}

#[test]
fn capture_unlisted_provider_is_unsupported() {
    let cases: Vec<(&str, i32, &str, Option<&str>)> = vec![
        ("Gecko", UIA_EDIT_CONTROL_TYPE, "", None),
        ("XAML", UIA_EDIT_CONTROL_TYPE, "TextBox", None),
        ("WPF", UIA_EDIT_CONTROL_TYPE, "TextBox", None),
        ("Win32", 50033, "TermControl", Some("TermControl")),
        (
            "Win32",
            50004,
            "ConsoleWindowClass",
            Some("ConsoleWindowClass"),
        ),
        // Chrome document (e.g. NVDA browse mode) is not an edit.
        ("Chrome", 50030, "", None),
        // Monaco / VS Code mirror only part of the document.
        (
            "Chrome",
            UIA_EDIT_CONTROL_TYPE,
            "inputarea monaco-mouse-cursor-text",
            None,
        ),
        ("", UIA_EDIT_CONTROL_TYPE, "Edit", Some("Edit")),
    ];
    for (framework, control_type, class, real) in cases {
        let mut f = edit_facts("x");
        f.framework_id = framework.into();
        f.control_type = control_type;
        f.class_name = class.into();
        f.real_class = real.map(Into::into);
        assert_eq!(
            classify_capture(&f),
            Err(CaptureError::UnsupportedTarget),
            "{framework}/{control_type}/{class}"
        );
    }
}

#[test]
fn capture_superclassed_edit_is_supported() {
    // Delphi TMemo: ClassName "TMemo", RealGetWindowClass "Edit".
    let mut f = edit_facts("x");
    f.class_name = "TMemo".into();
    f.real_class = Some("Edit".into());
    assert!(classify_capture(&f).is_ok());
    for real in ["RichEdit20W", "RICHEDIT50W", "richeditd2dpt"] {
        let mut f = edit_facts("x");
        f.real_class = Some(real.into());
        assert!(classify_capture(&f).is_ok(), "{real}");
    }
}

#[test]
fn capture_handy_window_is_error() {
    let mut f = edit_facts("x");
    f.foreground_pid = f.own_pid;
    f.focused_pid = f.own_pid;
    assert_eq!(classify_capture(&f), Err(CaptureError::HandyWindowFocused));
}

#[test]
fn capture_focus_outside_foreground_process_is_error() {
    let mut f = edit_facts("x");
    f.focused_pid = 200;
    assert_eq!(
        classify_capture(&f),
        Err(CaptureError::FocusOutsideForegroundApp)
    );
}

#[test]
fn capture_selection_limit_boundary() {
    // Astral scalars take two UTF-16 units each: 131072 scalars = 262144
    // units pass; one more scalar fails.
    let ok = "\u{1F600}".repeat(MAX_SELECTION_SCALARS);
    assert!(matches!(
        classify_capture(&edit_facts(&ok)),
        Ok(CapturedText::Selected(s)) if s.chars().count() == MAX_SELECTION_SCALARS
    ));
    let too_many = "\u{1F600}".repeat(MAX_SELECTION_SCALARS + 1);
    assert_eq!(
        classify_capture(&edit_facts(&too_many)),
        Err(CaptureError::SelectionTooLarge)
    );
    // BMP text: the scalar limit applies before the unit limit.
    let bmp = "a".repeat(MAX_SELECTION_SCALARS + 1);
    assert_eq!(
        classify_capture(&edit_facts(&bmp)),
        Err(CaptureError::SelectionTooLarge)
    );
    assert!(classify_capture(&edit_facts(&"a".repeat(MAX_SELECTION_SCALARS))).is_ok());
}

#[test]
fn capture_embedded_object_is_error() {
    assert_eq!(
        classify_capture(&edit_facts("before \u{FFFC} after")),
        Err(CaptureError::EmbeddedObject)
    );
}

#[test]
fn capture_nonempty_range_without_text_is_error() {
    let mut f = edit_facts("x");
    f.text = Some(Vec::new());
    assert_eq!(classify_capture(&f), Err(CaptureError::EmptyNonDegenerate));
    f.text = None;
    assert_eq!(classify_capture(&f), Err(CaptureError::EmptyNonDegenerate));
}

#[test]
fn capture_lone_surrogate_is_error() {
    let mut f = edit_facts("x");
    f.text = Some(vec![0x0061, 0xD83D, 0x0062]);
    assert_eq!(classify_capture(&f), Err(CaptureError::InvalidText));
}

#[test]
fn captured_text_debug_never_contains_content() {
    let shown = format!("{:?}", CapturedText::Selected("top secret".into()));
    assert!(!shown.contains("secret"), "{shown}");
}

// --- capture waiter --------------------------------------------------------

fn leaked_flag() -> &'static AtomicBool {
    Box::leak(Box::new(AtomicBool::new(false)))
}

type ReplySlot = Arc<Mutex<Option<CaptureReply>>>;

/// Starts a capture whose worker parks the reply in a slot.
fn start_parked(
    in_flight: &'static AtomicBool,
    timeout: Duration,
) -> (
    CaptureTicket,
    ReplySlot,
    Receiver<Result<Captured, CaptureError>>,
) {
    let slot: ReplySlot = Arc::new(Mutex::new(None));
    let (done_tx, done_rx) = mpsc::channel();
    let parked = slot.clone();
    let (parked_tx, parked_rx) = mpsc::channel();
    let ticket = begin_capture_with(
        in_flight,
        timeout,
        false,
        move |result| {
            let _ = done_tx.send(result);
        },
        move |reply| {
            *parked.lock().unwrap() = Some(reply);
            parked_tx.send(()).unwrap();
            Ok(())
        },
    );
    parked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    (ticket, slot, done_rx)
}

#[test]
fn capture_result_is_delivered_once() {
    let flag = leaked_flag();
    let (_ticket, slot, done) = start_parked(flag, Duration::from_secs(5));
    let (tok, _released) = token(1, 7);
    let reply = slot.lock().unwrap().take().unwrap();
    reply.send(Ok(Captured {
        text: CapturedText::Empty,
        token: tok,
    }));
    let result = done.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        result,
        Ok(Captured {
            text: CapturedText::Empty,
            ..
        })
    ));
    assert!(!flag.load(Ordering::SeqCst));
}

#[test]
fn capture_cancel_reports_cancelled_once() {
    let flag = leaked_flag();
    let (ticket, slot, done) = start_parked(flag, Duration::from_secs(5));
    ticket.cancel();
    ticket.cancel();
    let first = done.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(first, Err(CaptureError::Cancelled)));
    // A result after cancellation is not reported again.
    let (tok, released) = token(3, 7);
    slot.lock().unwrap().take().unwrap().send(Ok(Captured {
        text: CapturedText::Empty,
        token: tok,
    }));
    assert!(done.recv_timeout(Duration::from_millis(200)).is_err());
    assert_eq!(
        released.recv_timeout(Duration::from_secs(2)).unwrap(),
        TargetId(3)
    );
}

#[test]
fn capture_timeout_reports_timeout() {
    let flag = leaked_flag();
    let started = Instant::now();
    let (_ticket, _slot, done) = start_parked(flag, Duration::from_millis(50));
    let result = done.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(result, Err(CaptureError::Timeout)));
    assert!(started.elapsed() >= Duration::from_millis(50));
    // The worker still holds the request: admission stays closed.
    assert!(flag.load(Ordering::SeqCst));
}

#[test]
fn capture_late_result_is_released() {
    let flag = leaked_flag();
    let (_ticket, slot, done) = start_parked(flag, Duration::from_millis(30));
    assert!(matches!(
        done.recv_timeout(Duration::from_secs(2)).unwrap(),
        Err(CaptureError::Timeout)
    ));
    let (tok, released) = token(9, 7);
    slot.lock().unwrap().take().unwrap().send(Ok(Captured {
        text: CapturedText::Selected("late".into()),
        token: tok,
    }));
    assert_eq!(
        released.recv_timeout(Duration::from_secs(2)).unwrap(),
        TargetId(9)
    );
    assert!(done.recv_timeout(Duration::from_millis(100)).is_err());
    assert!(
        !flag.load(Ordering::SeqCst),
        "worker finished: admission reopens"
    );
}

#[test]
fn capture_busy_while_in_flight() {
    let flag = leaked_flag();
    let (_first, slot, _first_done) = start_parked(flag, Duration::from_secs(5));
    let submitted = Arc::new(AtomicUsize::new(0));

    let try_again = |submitted: &Arc<AtomicUsize>| {
        let (tx, rx) = mpsc::channel();
        let counter = submitted.clone();
        let _ticket = begin_capture_with(
            flag,
            Duration::from_secs(5),
            false,
            move |r| {
                let _ = tx.send(r);
            },
            move |reply| {
                counter.fetch_add(1, Ordering::SeqCst);
                reply.send(Err(CaptureError::NoCaret));
                Ok(())
            },
        );
        rx.recv_timeout(Duration::from_secs(2)).unwrap()
    };

    assert_eq!(try_again(&submitted).err(), Some(CaptureError::Busy));
    assert_eq!(
        submitted.load(Ordering::SeqCst),
        0,
        "busy press never reaches the worker"
    );

    // The worker finishes the first request: the next press is admitted.
    slot.lock()
        .unwrap()
        .take()
        .unwrap()
        .send(Err(CaptureError::NoCaret));
    assert_eq!(try_again(&submitted).err(), Some(CaptureError::NoCaret));
    assert_eq!(submitted.load(Ordering::SeqCst), 1);
}

#[test]
fn capture_refused_while_shutting_down_or_unavailable() {
    let flag = leaked_flag();
    let (tx, rx) = mpsc::channel();
    let _t = begin_capture_with(
        flag,
        Duration::from_secs(1),
        true,
        move |r| {
            let _ = tx.send(r);
        },
        |_| panic!("must not submit while shutting down"),
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap().err(),
        Some(CaptureError::ShuttingDown)
    );

    let (tx, rx) = mpsc::channel();
    let _t = begin_capture_with(
        flag,
        Duration::from_secs(1),
        false,
        move |r| {
            let _ = tx.send(r);
        },
        |_reply| Err(CaptureError::Unsupported),
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap().err(),
        Some(CaptureError::Unsupported)
    );
    assert!(
        !flag.load(Ordering::SeqCst),
        "failed submit drops its guard"
    );
}

// --- events ------------------------------------------------------------------

#[test]
fn event_foreground_invalidates() {
    assert_eq!(
        classify_event(EVENT_SYSTEM_FOREGROUND, 0),
        Some(InvalidationReason::Foreground)
    );
}

#[test]
fn event_caret_location_invalidates() {
    assert_eq!(
        classify_event(EVENT_OBJECT_LOCATIONCHANGE, OBJID_CARET),
        Some(InvalidationReason::Caret)
    );
}

#[test]
fn event_non_caret_location_ignored() {
    // OBJID_WINDOW (0) and OBJID_CLIENT (-4) move with scrolling/layout.
    assert_eq!(classify_event(EVENT_OBJECT_LOCATIONCHANGE, 0), None);
    assert_eq!(classify_event(EVENT_OBJECT_LOCATIONCHANGE, -4), None);
}

#[test]
fn event_value_change_ignored() {
    assert_eq!(classify_event(EVENT_OBJECT_VALUECHANGE, 0), None);
    assert_eq!(classify_event(EVENT_OBJECT_VALUECHANGE, -4), None);
}

#[test]
fn event_focus_and_text_selection_invalidate() {
    assert_eq!(
        classify_event(EVENT_OBJECT_FOCUS, -4),
        Some(InvalidationReason::Focus)
    );
    assert_eq!(
        classify_event(EVENT_OBJECT_TEXTSELECTIONCHANGED, -4),
        Some(InvalidationReason::TextSelection)
    );
}

#[test]
fn invalidation_keeps_first_reason() {
    let flag = AtomicU8::new(0);
    assert_eq!(invalidation_of(&flag), None);
    invalidate(&flag, InvalidationReason::Caret);
    invalidate(&flag, InvalidationReason::Superseded);
    assert_eq!(invalidation_of(&flag), Some(InvalidationReason::Caret));
    for reason in InvalidationReason::ALL {
        let f = AtomicU8::new(0);
        invalidate(&f, reason);
        assert_eq!(invalidation_of(&f), Some(reason));
    }
}

// --- validation --------------------------------------------------------------

fn identity(original: OriginalSelection) -> TargetIdentity {
    TargetIdentity {
        foreground_hwnd: 0x1000,
        pid: 100,
        thread_id: 55,
        process_created: 1_000_000,
        gui_focus_hwnd: 0x2000,
        runtime_id: vec![42, 7, 1],
        focused_pid: 100,
        original,
    }
}

/// Live state matching `identity(..)` unless a field is changed.
#[derive(Clone)]
struct FakeLive {
    foreground: usize,
    pid: u32,
    created: u64,
    gui_focus: usize,
    runtime_id: Vec<i32>,
    focused_pid: u32,
    count: i32,
    deltas: (i32, i32),
    text: Vec<u16>,
    degenerate: bool,
    reads: Vec<&'static str>,
    focus_error: Option<StaleReason>,
}

impl FakeLive {
    fn matching(selected: &str) -> Self {
        Self {
            foreground: 0x1000,
            pid: 100,
            created: 1_000_000,
            gui_focus: 0x2000,
            runtime_id: vec![42, 7, 1],
            focused_pid: 100,
            count: 1,
            deltas: (0, 0),
            text: utf16(selected),
            degenerate: selected.is_empty(),
            reads: Vec::new(),
            focus_error: None,
        }
    }
}

impl LiveTarget for FakeLive {
    fn foreground_hwnd(&mut self) -> usize {
        self.reads.push("foreground");
        self.foreground
    }
    fn process_identity(&mut self, _hwnd: usize) -> Result<(u32, u64), StaleReason> {
        self.reads.push("process");
        Ok((self.pid, self.created))
    }
    fn gui_focus_hwnd(&mut self, _thread_id: u32) -> usize {
        self.reads.push("gui_focus");
        self.gui_focus
    }
    fn focused_element(&mut self) -> Result<(Vec<i32>, u32), StaleReason> {
        self.reads.push("element");
        if let Some(e) = self.focus_error.clone() {
            return Err(e);
        }
        Ok((self.runtime_id.clone(), self.focused_pid))
    }
    fn selection_count(&mut self) -> Result<i32, StaleReason> {
        self.reads.push("count");
        Ok(self.count)
    }
    fn endpoint_deltas(&mut self) -> Result<(i32, i32), StaleReason> {
        self.reads.push("endpoints");
        Ok(self.deltas)
    }
    fn selection_text(&mut self, max_len: i32) -> Result<Vec<u16>, StaleReason> {
        self.reads.push("text");
        let max = usize::try_from(max_len).unwrap();
        Ok(self.text.iter().copied().take(max).collect())
    }
    fn selection_degenerate(&mut self) -> Result<bool, StaleReason> {
        self.reads.push("degenerate");
        Ok(self.degenerate)
    }
}

fn selected_identity(text: &str) -> TargetIdentity {
    identity(OriginalSelection::Selected(utf16(text)))
}

#[test]
fn validate_same_evidence_passes() {
    let mut live = FakeLive::matching("hello\r\nworld");
    assert_eq!(
        compare_evidence(&selected_identity("hello\r\nworld"), None, &mut live),
        Ok(())
    );
    let mut live = FakeLive::matching("");
    assert_eq!(
        compare_evidence(&identity(OriginalSelection::Empty), None, &mut live),
        Ok(())
    );
}

#[test]
fn validate_foreground_changed() {
    let mut live = FakeLive::matching("x");
    live.foreground = 0x9999;
    assert_eq!(
        compare_evidence(&selected_identity("x"), None, &mut live),
        Err(StaleReason::ForegroundChanged)
    );
    assert_eq!(live.reads, vec!["foreground"], "stops at first mismatch");
}

#[test]
fn validate_reused_pid_different_start_time() {
    let mut live = FakeLive::matching("x");
    live.created += 1;
    assert_eq!(
        compare_evidence(&selected_identity("x"), None, &mut live),
        Err(StaleReason::ProcessChanged)
    );
    let mut live = FakeLive::matching("x");
    live.pid = 101;
    assert_eq!(
        compare_evidence(&selected_identity("x"), None, &mut live),
        Err(StaleReason::ProcessChanged)
    );
}

#[test]
fn validate_gui_focus_changed() {
    let mut live = FakeLive::matching("x");
    live.gui_focus = 0x3000;
    assert_eq!(
        compare_evidence(&selected_identity("x"), None, &mut live),
        Err(StaleReason::FocusChanged)
    );
    // Without a recorded GUI focus window the check is skipped.
    let mut id = selected_identity("x");
    id.gui_focus_hwnd = 0;
    assert_eq!(compare_evidence(&id, None, &mut live), Ok(()));
}

#[test]
fn validate_runtime_id_changed() {
    let mut live = FakeLive::matching("x");
    live.runtime_id = vec![42, 7, 2];
    assert_eq!(
        compare_evidence(&selected_identity("x"), None, &mut live),
        Err(StaleReason::FocusChanged)
    );
    let mut live = FakeLive::matching("x");
    live.focused_pid = 300;
    assert_eq!(
        compare_evidence(&selected_identity("x"), None, &mut live),
        Err(StaleReason::FocusChanged)
    );
}

#[test]
fn validate_endpoint_moved() {
    for deltas in [(1, 0), (0, -1), (-3, 2)] {
        let mut live = FakeLive::matching("x");
        live.deltas = deltas;
        assert_eq!(
            compare_evidence(&selected_identity("x"), None, &mut live),
            Err(StaleReason::SelectionChanged),
            "{deltas:?}"
        );
    }
    let mut live = FakeLive::matching("x");
    live.count = 0;
    assert_eq!(
        compare_evidence(&selected_identity("x"), None, &mut live),
        Err(StaleReason::SelectionChanged)
    );
}

#[test]
fn validate_same_text_different_position() {
    // The user selected an identical word elsewhere: text matches, the
    // range endpoints do not.
    let mut live = FakeLive::matching("word");
    live.deltas = (12, 12);
    assert_eq!(
        compare_evidence(&selected_identity("word"), None, &mut live),
        Err(StaleReason::SelectionChanged)
    );
    assert!(!live.reads.contains(&"text"));
}

#[test]
fn validate_text_changed_same_range() {
    let mut live = FakeLive::matching("word!");
    assert_eq!(
        compare_evidence(&selected_identity("word"), None, &mut live),
        Err(StaleReason::TextChanged),
        "a longer fresh text must not pass as a prefix match"
    );
    let mut live = FakeLive::matching("Word");
    assert_eq!(
        compare_evidence(&selected_identity("word"), None, &mut live),
        Err(StaleReason::TextChanged)
    );
}

#[test]
fn validate_empty_no_longer_collapsed() {
    let mut live = FakeLive::matching("");
    live.degenerate = false;
    assert_eq!(
        compare_evidence(&identity(OriginalSelection::Empty), None, &mut live),
        Err(StaleReason::NotCollapsed)
    );
}

#[test]
fn validate_invalidation_wins_over_matching_evidence() {
    // The user moved away and came back to identical content.
    let mut live = FakeLive::matching("x");
    assert_eq!(
        compare_evidence(
            &selected_identity("x"),
            Some(InvalidationReason::Foreground),
            &mut live
        ),
        Err(StaleReason::Invalidated(InvalidationReason::Foreground))
    );
    assert!(live.reads.is_empty());
}

#[test]
fn validate_provider_error_propagates() {
    let mut live = FakeLive::matching("x");
    live.focus_error = Some(StaleReason::Unresponsive);
    assert_eq!(
        compare_evidence(&selected_identity("x"), None, &mut live),
        Err(StaleReason::Unresponsive)
    );
}

// --- commit ------------------------------------------------------------------

fn receipt_settlement() -> StrictSettlement {
    StrictSettlement {
        receipt_after_injection: true,
        ownership_lost: false,
        preempted: false,
        clipboard: ClipboardSettlement::Restored,
    }
}

/// Scriptable `CommitPlatform` that records its calls.
struct FakePlatform {
    calls: Vec<String>,
    validations: VecDeque<Result<(), StaleReason>>,
    begin: Option<BeginClipboardError>,
    still_owned: bool,
    modifier_polls_held: usize,
    modifier_polls: std::cell::Cell<usize>,
    /// Answers of successive `clipboard_free` calls, then the default.
    clipboard_free_script: std::cell::RefCell<VecDeque<bool>>,
    clipboard_free_default: bool,
    calls_free: std::cell::Cell<usize>,
    on_first_free_check: Option<Box<dyn Fn()>>,
    on_boundary_validate: Option<Box<dyn Fn()>>,
    modifier_after_boundary: std::cell::Cell<bool>,
    hold_modifier_at_boundary: bool,
    chord: ChordResult,
    abort_result: ClipboardSettlement,
    abort_hook: Option<fn() -> ClipboardSettlement>,
    settlement: Option<StrictSettlement>,
    settle_delay: Duration,
    verified: bool,
    published: Option<String>,
    /// Flipped right after injection to prove late cancel is ignored.
    cancel_after_inject: Option<&'static AtomicBool>,
}

impl FakePlatform {
    fn new() -> Self {
        Self {
            calls: Vec::new(),
            validations: VecDeque::new(),
            begin: None,
            still_owned: true,
            modifier_polls_held: 0,
            modifier_polls: std::cell::Cell::new(0),
            clipboard_free_script: std::cell::RefCell::new(VecDeque::new()),
            clipboard_free_default: true,
            calls_free: std::cell::Cell::new(0),
            on_first_free_check: None,
            on_boundary_validate: None,
            modifier_after_boundary: std::cell::Cell::new(false),
            hold_modifier_at_boundary: false,
            chord: ChordResult::Sent,
            abort_result: ClipboardSettlement::Restored,
            abort_hook: None,
            settlement: Some(receipt_settlement()),
            settle_delay: Duration::ZERO,
            verified: true,
            published: None,
            cancel_after_inject: None,
        }
    }

    fn injected(&self) -> bool {
        self.calls.iter().any(|c| c == "inject")
    }
}

impl CommitPlatform for FakePlatform {
    fn validate(
        &mut self,
        _id: TargetId,
        boundary: bool,
        deadline: Instant,
    ) -> Result<(), StaleReason> {
        assert!(deadline > Instant::now());
        self.calls.push(format!("validate(boundary={boundary})"));
        if boundary {
            self.modifier_after_boundary
                .set(self.hold_modifier_at_boundary);
            if let Some(on_validate) = &self.on_boundary_validate {
                on_validate();
            }
        }
        self.validations.pop_front().unwrap_or(Ok(()))
    }
    fn begin_clipboard(
        &mut self,
        text: &str,
        _after_success: ClipboardHandling,
    ) -> Result<(), BeginClipboardError> {
        self.calls.push("begin_clipboard".into());
        match self.begin.take() {
            Some(e) => Err(e),
            None => {
                self.published = Some(text.to_string());
                Ok(())
            }
        }
    }
    fn clipboard_still_owned(&self) -> bool {
        self.still_owned
    }
    fn clipboard_free(&self) -> bool {
        let mut script = self.clipboard_free_script.borrow_mut();
        let free = script.pop_front().unwrap_or(self.clipboard_free_default);
        self.calls_free.set(self.calls_free.get() + 1);
        if self.calls_free.get() == 1 {
            if let Some(on_check) = &self.on_first_free_check {
                on_check();
            }
        }
        free
    }
    fn modifiers_held(&self) -> bool {
        let polls = self.modifier_polls.get();
        self.modifier_polls.set(polls + 1);
        polls < self.modifier_polls_held || self.modifier_after_boundary.get()
    }
    fn inject_paste(&mut self) -> ChordResult {
        self.calls.push("inject".into());
        if let Some(flag) = self.cancel_after_inject {
            flag.store(true, Ordering::SeqCst);
        }
        self.chord.clone()
    }
    fn abort_clipboard(&mut self) -> ClipboardSettlement {
        self.calls.push("abort".into());
        if let Some(abort) = self.abort_hook {
            return abort();
        }
        self.abort_result.clone()
    }
    fn await_settlement(&mut self) -> Option<StrictSettlement> {
        self.calls.push("await_settlement".into());
        std::thread::sleep(self.settle_delay);
        self.settlement.clone()
    }
    fn verify_inserted(&mut self, _id: TargetId, text: &str, _deadline: Instant) -> bool {
        self.calls.push(format!("verify({})", text.len()));
        self.verified
    }
}

fn not_cancelled() -> bool {
    false
}

fn run(p: &mut FakePlatform, replacement: &str) -> (CommitOutcome, Receiver<TargetId>) {
    let (tok, released) = token(5, 1);
    let outcome = run_commit(
        p,
        CommitRequest {
            token: tok,
            session: SessionId(1),
            replacement,
            clipboard_handling: ClipboardHandling::DontModify,
            is_cancelled: &not_cancelled,
        },
        &|| false,
    );
    (outcome, released)
}

#[test]
fn commit_receipt_and_verified_text_is_delivered() {
    let mut p = FakePlatform::new();
    let (outcome, _) = run(&mut p, "new text");
    assert_eq!(
        outcome,
        CommitOutcome::Delivered {
            clipboard: ClipboardSettlement::Restored
        }
    );
    assert_eq!(
        p.calls,
        vec![
            "validate(boundary=false)",
            "begin_clipboard",
            "validate(boundary=true)",
            "inject",
            "await_settlement",
            "verify(8)",
        ]
    );
}

#[test]
fn commit_publishes_replacement_verbatim() {
    let mut p = FakePlatform::new();
    let text = "  line one\nline two  ";
    let _ = run(&mut p, text);
    // No trailing space, no trimming, no newline conversion.
    assert_eq!(p.published.as_deref(), Some(text));
}

#[test]
fn commit_refuses_stale_before_clipboard() {
    let mut p = FakePlatform::new();
    p.validations
        .push_back(Err(StaleReason::Invalidated(InvalidationReason::Focus)));
    let (outcome, _) = run(&mut p, "x");
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::TargetStale(StaleReason::Invalidated(InvalidationReason::Focus)),
            clipboard: ClipboardSettlement::Untouched,
        }
    );
    assert_eq!(p.calls, vec!["validate(boundary=false)"]);
}

#[test]
fn commit_refuses_invalidated_token_without_platform_calls() {
    let mut p = FakePlatform::new();
    let (tok, _released) = token(5, 1);
    invalidate(&tok.invalidation, InvalidationReason::Caret);
    let outcome = run_commit(
        &mut p,
        CommitRequest {
            token: tok,
            session: SessionId(1),
            replacement: "x",
            clipboard_handling: ClipboardHandling::DontModify,
            is_cancelled: &not_cancelled,
        },
        &|| false,
    );
    assert!(matches!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::TargetStale(StaleReason::Invalidated(InvalidationReason::Caret)),
            clipboard: ClipboardSettlement::Untouched
        }
    ));
    assert!(p.calls.is_empty());
}

#[test]
fn commit_refuses_lossy_clipboard_before_injection() {
    let mut p = FakePlatform::new();
    let lossy = LossyFormat {
        format: 14,
        reason: LossyReason::NonMemoryHandle,
    };
    p.begin = Some(BeginClipboardError::Refused(
        RefuseReason::ClipboardNotPreservable(lossy.clone()),
        ClipboardSettlement::Untouched,
    ));
    let (outcome, _) = run(&mut p, "x");
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::ClipboardNotPreservable(lossy),
            clipboard: ClipboardSettlement::Untouched,
        }
    );
    assert!(!p.injected());
}

#[test]
fn commit_publish_failure_reports_restore() {
    let mut p = FakePlatform::new();
    p.begin = Some(BeginClipboardError::Failed(
        FailReason::PublishFailed("SetClipboardData failed".into()),
        ClipboardSettlement::Restored,
    ));
    let (outcome, _) = run(&mut p, "x");
    assert_eq!(
        outcome,
        CommitOutcome::Failed {
            reason: FailReason::PublishFailed("SetClipboardData failed".into()),
            clipboard: ClipboardSettlement::Restored,
        }
    );
    assert!(!p.injected());
}

#[test]
fn commit_revalidates_at_injection_boundary() {
    let mut p = FakePlatform::new();
    p.validations.push_back(Ok(()));
    p.validations.push_back(Err(StaleReason::SelectionChanged));
    let (outcome, _) = run(&mut p, "x");
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::TargetStale(StaleReason::SelectionChanged),
            clipboard: ClipboardSettlement::Restored,
        }
    );
    assert_eq!(
        p.calls,
        vec![
            "validate(boundary=false)",
            "begin_clipboard",
            "validate(boundary=true)",
            "abort",
        ]
    );
}

#[test]
fn commit_refuses_when_clipboard_changed_before_injection() {
    let mut p = FakePlatform::new();
    p.still_owned = false;
    p.abort_result = ClipboardSettlement::ExternallyChanged;
    let (outcome, _) = run(&mut p, "x");
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::ClipboardChanged,
            clipboard: ClipboardSettlement::ExternallyChanged,
        }
    );
    assert!(!p.injected());
}

#[test]
fn commit_cancel_before_injection_restores() {
    let mut p = FakePlatform::new();
    let (tok, _released) = token(5, 1);
    let calls = AtomicUsize::new(0);
    // Not cancelled at entry; cancelled by the time the clipboard is ready.
    let cancelled = move || calls.fetch_add(1, Ordering::SeqCst) >= 1;
    let outcome = run_commit(
        &mut p,
        CommitRequest {
            token: tok,
            session: SessionId(1),
            replacement: "x",
            clipboard_handling: ClipboardHandling::DontModify,
            is_cancelled: &cancelled,
        },
        &|| false,
    );
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::Cancelled,
            clipboard: ClipboardSettlement::Restored,
        }
    );
    assert!(p.calls.contains(&"abort".to_string()));
    assert!(!p.injected());
}

#[test]
fn commit_refuses_held_modifiers_after_wait() {
    let mut p = FakePlatform::new();
    p.modifier_polls_held = usize::MAX;
    let started = Instant::now();
    let (outcome, _) = run(&mut p, "x");
    assert!(started.elapsed() >= MODIFIER_WAIT);
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::ModifiersHeld,
            clipboard: ClipboardSettlement::Restored,
        }
    );
    assert!(!p.injected());

    // Modifiers released within the wait: the commit proceeds.
    let mut p = FakePlatform::new();
    p.modifier_polls_held = 3;
    let (outcome, _) = run(&mut p, "x");
    assert!(matches!(outcome, CommitOutcome::Delivered { .. }));
    assert_eq!(p.modifier_polls.get(), 5); // release wait plus injection-boundary check
}

#[test]
fn commit_waits_for_held_clipboard_then_injects() {
    let mut p = FakePlatform::new();
    // Held twice, then free; the final pre-injection check is free too.
    p.clipboard_free_script
        .borrow_mut()
        .extend([false, false, true]);
    let (outcome, _) = run(&mut p, "x");
    assert!(matches!(outcome, CommitOutcome::Delivered { .. }));
    assert_eq!(p.calls_free.get(), 4, "3 waiting polls + 1 final check");
    assert!(p.injected());
}

#[test]
fn commit_held_clipboard_past_deadline_refuses_and_restores() {
    let mut p = FakePlatform::new();
    p.clipboard_free_default = false;
    let started = Instant::now();
    let (outcome, released) = run(&mut p, "x");
    assert!(started.elapsed() >= CLIPBOARD_FREE_WAIT);
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::ClipboardBusy,
            clipboard: ClipboardSettlement::Restored,
        }
    );
    assert!(
        !p.injected(),
        "nothing may be sent while the clipboard is held"
    );
    assert_eq!(p.calls.last().map(String::as_str), Some("abort"));
    // Waiting happens before the boundary validation.
    assert!(!p.calls.contains(&"validate(boundary=true)".to_string()));
    assert_eq!(released.try_recv().ok(), Some(TargetId(5)));
}

#[test]
fn commit_clipboard_grabbed_during_boundary_validation_refuses() {
    let mut p = FakePlatform::new();
    p.clipboard_free_script.borrow_mut().extend([true, false]);
    let (outcome, _) = run(&mut p, "x");
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::ClipboardBusy,
            clipboard: ClipboardSettlement::Restored,
        }
    );
    assert!(p.calls.contains(&"validate(boundary=true)".to_string()));
    assert!(!p.injected());
}

#[test]
fn commit_cancel_or_shutdown_during_holder_wait_restores() {
    for shutdown in [false, true] {
        let changed = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&changed);
        let mut p = FakePlatform::new();
        p.clipboard_free_script.borrow_mut().extend([false, true]);
        p.on_first_free_check = Some(Box::new(move || signal.store(true, Ordering::SeqCst)));
        let (tok, released) = token(5, 1);
        let outcome = run_commit(
            &mut p,
            CommitRequest {
                token: tok,
                session: SessionId(1),
                replacement: "recoverable replacement",
                clipboard_handling: ClipboardHandling::DontModify,
                is_cancelled: &|| !shutdown && changed.load(Ordering::SeqCst),
            },
            &|| shutdown && changed.load(Ordering::SeqCst),
        );
        assert_eq!(
            outcome,
            CommitOutcome::Refused {
                reason: if shutdown {
                    RefuseReason::Shutdown
                } else {
                    RefuseReason::Cancelled
                },
                clipboard: ClipboardSettlement::Restored,
            }
        );
        assert!(!p.injected());
        assert_eq!(p.calls.last().map(String::as_str), Some("abort"));
        assert_eq!(released.try_recv().ok(), Some(TargetId(5)));
    }
}

#[test]
fn commit_injection_not_sent_fails_and_restores() {
    let mut p = FakePlatform::new();
    p.chord = ChordResult::NotSent("0 of 4 key events injected".into());
    let (outcome, _) = run(&mut p, "x");
    assert_eq!(
        outcome,
        CommitOutcome::Failed {
            reason: FailReason::InjectionNotSent("0 of 4 key events injected".into()),
            clipboard: ClipboardSettlement::Restored,
        }
    );
    assert_eq!(p.calls.last().map(String::as_str), Some("abort"));
}

#[test]
fn commit_cancel_or_shutdown_during_final_validation_restores() {
    for shutdown in [false, true] {
        let stop = Arc::new(AtomicBool::new(false));
        let boundary_stop = stop.clone();
        let mut p = FakePlatform::new();
        p.on_boundary_validate = Some(Box::new(move || {
            boundary_stop.store(true, Ordering::SeqCst)
        }));
        let (tok, released) = token(5, 1);
        let outcome = run_commit(
            &mut p,
            CommitRequest {
                token: tok,
                session: SessionId(1),
                replacement: "recoverable",
                clipboard_handling: ClipboardHandling::DontModify,
                is_cancelled: &|| !shutdown && stop.load(Ordering::SeqCst),
            },
            &|| shutdown && stop.load(Ordering::SeqCst),
        );
        assert_eq!(
            outcome,
            CommitOutcome::Refused {
                reason: if shutdown {
                    RefuseReason::Shutdown
                } else {
                    RefuseReason::Cancelled
                },
                clipboard: ClipboardSettlement::Restored,
            }
        );
        assert!(!p.injected());
        assert_eq!(released.try_recv().ok(), Some(TargetId(5)));
    }
}

#[test]
fn validate_drains_change_and_change_back_before_disarming() {
    let flag = AtomicU8::new(0);
    let disarmed = std::cell::Cell::new(false);
    let result = evidence::finish_validation(
        Ok(()),
        true,
        &flag,
        || invalidate(&flag, InvalidationReason::TextSelection),
        || disarmed.set(true),
    );
    assert_eq!(
        result,
        Err(StaleReason::Invalidated(InvalidationReason::TextSelection))
    );
    assert!(
        !disarmed.get(),
        "pending selection events must be checked before unhooking"
    );
}

#[test]
fn verify_empty_target_rejects_preexisting_text_at_unchanged_caret() {
    assert!(!evidence::verify_insertion(true, 0, "x", "x"));
    assert!(!evidence::verify_insertion(true, -1, "x", "x"));
    assert!(evidence::verify_insertion(true, 1, "x", "x"));
    // Selected ranges can be adjusted by the provider during replacement.
    assert!(evidence::verify_insertion(false, 0, "x", "x"));
}

#[test]
fn shutdown_invalidates_live_token_before_worker_dequeues() {
    let stop = AtomicBool::new(false);
    let flag = Arc::new(AtomicU8::new(0));
    let target = Mutex::new(Some(Arc::downgrade(&flag)));
    signal_shutdown(&stop, &target);
    assert!(stop.load(Ordering::SeqCst));
    assert_eq!(invalidation_of(&flag), Some(InvalidationReason::Shutdown));
}

#[test]
fn commit_modifier_pressed_during_final_validation_restores() {
    let mut p = FakePlatform::new();
    p.hold_modifier_at_boundary = true;
    let (outcome, _) = run(&mut p, "recoverable");
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::ModifiersHeld,
            clipboard: ClipboardSettlement::Restored,
        }
    );
    assert!(!p.injected());
}

#[test]
fn capture_blocking_submission_is_off_caller_and_expires_from_entry() {
    let flag = leaked_flag();
    let (done_tx, done_rx) = mpsc::channel();
    let (unblock_tx, unblock_rx) = mpsc::channel();
    let (tok, released) = token(93, 1);
    let (call_returned_tx, call_returned_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let ticket = begin_capture_with(
            flag,
            Duration::from_millis(100),
            false,
            move |result| {
                done_tx.send(result).unwrap();
            },
            move |reply| {
                unblock_rx.recv().unwrap();
                reply.send(Ok(Captured {
                    text: CapturedText::Empty,
                    token: tok,
                }));
                Ok(())
            },
        );
        call_returned_tx.send(ticket).unwrap();
    });
    let returned = call_returned_rx.recv_timeout(Duration::from_millis(300));
    let result = done_rx.recv_timeout(Duration::from_millis(300));
    // Always release the simulated startup, including on the RED path.
    unblock_tx.send(()).unwrap();
    assert!(
        returned.is_ok(),
        "capture blocked its caller during submission"
    );
    assert!(
        matches!(result, Ok(Err(CaptureError::Timeout))),
        "late startup must expire from entry"
    );
    assert_eq!(
        released.recv_timeout(Duration::from_secs(2)).unwrap(),
        TargetId(93)
    );
    assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
}

#[test]
fn commit_ownership_change_between_check_and_clipboard_lock_preserves_newer_copy() {
    let mut p = FakePlatform::new();
    p.chord = ChordResult::NotSent("no paste events injected".into());
    p.abort_hook = Some(crate::paste_tx::tests::settle_with_external_copy_during_open);
    let (outcome, released) = run(&mut p, "recoverable replacement");
    assert_eq!(
        outcome,
        CommitOutcome::Failed {
            reason: FailReason::InjectionNotSent("no paste events injected".into()),
            clipboard: ClipboardSettlement::ExternallyChanged,
        }
    );
    assert_eq!(released.try_recv().ok(), Some(TargetId(5)));
}

#[test]
fn commit_partial_injection_is_uncertain() {
    let mut p = FakePlatform::new();
    p.chord = ChordResult::Partial("2 of 4 key events injected".into());
    let (outcome, _) = run(&mut p, "x");
    assert_eq!(
        outcome,
        CommitOutcome::Uncertain {
            reason: UncertainReason::PartialInjection("2 of 4 key events injected".into()),
            clipboard: ClipboardSettlement::Restored,
        }
    );
    assert!(!p.calls.iter().any(|c| c.starts_with("verify")));
}

#[test]
fn commit_waits_for_settlement_ack() {
    let mut p = FakePlatform::new();
    p.settle_delay = Duration::from_millis(150);
    p.settlement = Some(StrictSettlement {
        clipboard: ClipboardSettlement::LeftReplacement,
        ..receipt_settlement()
    });
    let started = Instant::now();
    let (outcome, _) = run(&mut p, "x");
    assert!(started.elapsed() >= Duration::from_millis(150));
    assert_eq!(
        outcome,
        CommitOutcome::Delivered {
            clipboard: ClipboardSettlement::LeftReplacement
        }
    );
}

#[test]
fn commit_missing_settlement_ack_is_uncertain() {
    let mut p = FakePlatform::new();
    p.settlement = None;
    let (outcome, _) = run(&mut p, "x");
    assert_eq!(
        outcome,
        CommitOutcome::Uncertain {
            reason: UncertainReason::SettlementUnacknowledged,
            clipboard: ClipboardSettlement::Unknown,
        }
    );
}

#[test]
fn commit_receipt_without_verification_is_uncertain() {
    let mut p = FakePlatform::new();
    p.verified = false;
    let (outcome, _) = run(&mut p, "x");
    assert_eq!(
        outcome,
        CommitOutcome::Uncertain {
            reason: UncertainReason::NotVerified,
            clipboard: ClipboardSettlement::Restored,
        }
    );
}

#[test]
fn commit_no_receipt_is_uncertain() {
    let mut p = FakePlatform::new();
    p.settlement = Some(StrictSettlement {
        receipt_after_injection: false,
        ..receipt_settlement()
    });
    let (outcome, _) = run(&mut p, "x");
    assert_eq!(
        outcome,
        CommitOutcome::Uncertain {
            reason: UncertainReason::NoReceipt,
            clipboard: ClipboardSettlement::Restored,
        }
    );
    let mut p = FakePlatform::new();
    p.settlement = Some(StrictSettlement {
        receipt_after_injection: false,
        preempted: true,
        ..receipt_settlement()
    });
    assert!(matches!(
        run(&mut p, "x").0,
        CommitOutcome::Uncertain {
            reason: UncertainReason::Preempted,
            ..
        }
    ));
}

#[test]
fn commit_ownership_lost_without_receipt_is_uncertain() {
    let mut p = FakePlatform::new();
    p.settlement = Some(StrictSettlement {
        receipt_after_injection: false,
        ownership_lost: true,
        preempted: false,
        clipboard: ClipboardSettlement::ExternallyChanged,
    });
    let (outcome, _) = run(&mut p, "x");
    assert_eq!(
        outcome,
        CommitOutcome::Uncertain {
            reason: UncertainReason::ClipboardChangedDuringPaste,
            clipboard: ClipboardSettlement::ExternallyChanged,
        }
    );
}

#[test]
fn commit_session_mismatch_and_empty_replacement_refused() {
    let mut p = FakePlatform::new();
    let (tok, _r) = token(5, 1);
    let outcome = run_commit(
        &mut p,
        CommitRequest {
            token: tok,
            session: SessionId(2),
            replacement: "x",
            clipboard_handling: ClipboardHandling::DontModify,
            is_cancelled: &not_cancelled,
        },
        &|| false,
    );
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::SessionMismatch,
            clipboard: ClipboardSettlement::Untouched,
        }
    );
    let (outcome, _) = run(&mut p, "");
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::EmptyReplacement,
            clipboard: ClipboardSettlement::Untouched,
        }
    );
    assert!(p.calls.is_empty());
}

#[test]
fn commit_after_shutdown_refused() {
    let mut p = FakePlatform::new();
    let (tok, _r) = token(5, 1);
    let outcome = run_commit(
        &mut p,
        CommitRequest {
            token: tok,
            session: SessionId(1),
            replacement: "x",
            clipboard_handling: ClipboardHandling::DontModify,
            is_cancelled: &not_cancelled,
        },
        &|| true,
    );
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::Shutdown,
            clipboard: ClipboardSettlement::Untouched,
        }
    );
    assert!(p.calls.is_empty());

    // Shutdown that starts after publishing aborts before injection.
    let mut p = FakePlatform::new();
    let (tok, _r) = token(5, 1);
    let checks = AtomicUsize::new(0);
    let outcome = run_commit(
        &mut p,
        CommitRequest {
            token: tok,
            session: SessionId(1),
            replacement: "x",
            clipboard_handling: ClipboardHandling::DontModify,
            is_cancelled: &not_cancelled,
        },
        &|| checks.fetch_add(1, Ordering::SeqCst) >= 1,
    );
    assert_eq!(
        outcome,
        CommitOutcome::Refused {
            reason: RefuseReason::Shutdown,
            clipboard: ClipboardSettlement::Restored,
        }
    );
    assert!(!p.injected());
}

#[test]
fn commit_releases_target_on_every_outcome() {
    let scenarios: [fn(&mut FakePlatform); 8] = [
        |_| {},
        |p| p.validations.push_back(Err(StaleReason::FocusChanged)),
        |p| {
            p.begin = Some(BeginClipboardError::Refused(
                RefuseReason::ClipboardBusy,
                ClipboardSettlement::Untouched,
            ))
        },
        |p| p.chord = ChordResult::NotSent("no".into()),
        |p| p.chord = ChordResult::Partial("half".into()),
        |p| p.settlement = None,
        |p| p.verified = false,
        |p| p.still_owned = false,
    ];
    for (i, scenario) in scenarios.iter().enumerate() {
        let mut p = FakePlatform::new();
        scenario(&mut p);
        let (_outcome, released) = run(&mut p, "x");
        assert_eq!(
            released.recv_timeout(Duration::from_secs(1)).ok(),
            Some(TargetId(5)),
            "scenario {i}"
        );
    }
    // Early refusal paths release as well.
    let mut p = FakePlatform::new();
    let (_outcome, released) = run(&mut p, "");
    assert_eq!(released.try_recv().ok(), Some(TargetId(5)));
}

#[test]
fn commit_ignores_cancel_after_injection() {
    static CANCEL: AtomicBool = AtomicBool::new(false);
    CANCEL.store(false, Ordering::SeqCst);
    let mut p = FakePlatform::new();
    p.cancel_after_inject = Some(&CANCEL);
    let (tok, _r) = token(5, 1);
    let is_cancelled = || CANCEL.load(Ordering::SeqCst);
    let outcome = run_commit(
        &mut p,
        CommitRequest {
            token: tok,
            session: SessionId(1),
            replacement: "x",
            clipboard_handling: ClipboardHandling::DontModify,
            is_cancelled: &is_cancelled,
        },
        &|| false,
    );
    assert!(matches!(outcome, CommitOutcome::Delivered { .. }));
    assert!(!p.calls.contains(&"abort".to_string()));
}

#[test]
fn verify_normalizes_crlf_and_nbsp() {
    assert!(inserted_text_matches("prefix a\r\nb c", "a\nb c"));
    assert!(inserted_text_matches("xa\rb", "a\nb"));
    assert!(inserted_text_matches("x\u{00A0}y", "x y"));
    assert!(inserted_text_matches("before new", "new"));
    assert!(!inserted_text_matches("before new!", "new"));
    assert!(!inserted_text_matches("ne", "new"));
    assert!(!inserted_text_matches("anything", ""));
    // Span covers the text, one extra unit per LF (CRLF), plus one.
    assert_eq!(verify_span("ab"), 3);
    assert_eq!(verify_span("a\nb"), 5);
    assert_eq!(verify_span("\u{1F600}"), 3);
}

#[test]
fn reason_codes_are_stable_snake_case() {
    let codes = [
        CaptureError::SelectionTooLarge.code(),
        StaleReason::Invalidated(InvalidationReason::Caret).code(),
        RefuseReason::ClipboardNotPreservable(LossyFormat {
            format: 14,
            reason: LossyReason::NonMemoryHandle,
        })
        .code(),
        FailReason::InjectionNotSent(String::new()).code(),
        UncertainReason::NotVerified.code(),
        RefuseReason::WorkerUnavailable.code(),
        InvalidationReason::TextSelection.code(),
    ];
    for code in codes {
        assert!(
            code.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "{code}"
        );
    }
    assert_eq!(
        RefuseReason::ClipboardNotPreservable(LossyFormat {
            format: 14,
            reason: LossyReason::NonMemoryHandle
        })
        .code(),
        "clipboard_not_preservable"
    );
}

// --- live probe (manual, Windows) -------------------------------------------

/// Child process of `probe_live_target`: hosts a classic edit control.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "child process of probe_live_target"]
fn probe_edit_host() {
    if let Ok(class) = std::env::var("HANDY_PROBE_EDIT_HOST") {
        super::probe::run_edit_host(&class);
    }
}

#[cfg(target_os = "windows")]
struct ProbeReport(Vec<String>);

#[cfg(target_os = "windows")]
impl ProbeReport {
    fn row(&mut self, target: &str, case: &str, detail: impl AsRef<str>) {
        let line = format!("| {target} | {case} | {} |", detail.as_ref());
        println!("{line}");
        self.0.push(line);
    }
}

#[cfg(target_os = "windows")]
fn describe_capture(result: &Result<Captured, CaptureError>, elapsed: Duration) -> String {
    match result {
        Ok(c) => match &c.text {
            CapturedText::Selected(s) if s.len() > 256 => format!(
                "Selected ({} scalars, {} UTF-16 units) in {} ms",
                s.chars().count(),
                s.encode_utf16().count(),
                elapsed.as_millis()
            ),
            _ => format!("{:?} in {} ms", c.text, elapsed.as_millis()),
        },
        Err(e) => format!("error {} ({e:?}) in {} ms", e.code(), elapsed.as_millis()),
    }
}

#[cfg(target_os = "windows")]
fn describe_token(token: &TargetToken) -> String {
    format!(
        "invalidation={:?} validate={:?}",
        token.invalidation(),
        super::windows::probe_validate(token.id())
    )
}

#[cfg(target_os = "windows")]
fn probe_commit(
    report: &mut ProbeReport,
    target: &str,
    case: &str,
    token: TargetToken,
    replacement: &str,
) -> CommitOutcome {
    use super::probe::clipboard_digest;
    use crate::paste_tx::diag;
    super::probe::check_owned_foreground().unwrap();
    let _ = diag::take();
    let session = token.session();
    let before = clipboard_digest();
    let before_again = clipboard_digest();
    let started = Instant::now();
    let outcome = commit(CommitRequest {
        token,
        session,
        replacement,
        clipboard_handling: ClipboardHandling::DontModify,
        is_cancelled: &|| false,
    });
    let elapsed = started.elapsed();
    let events = diag::take();
    let mut last_check = None;
    let mut injecting = None;
    let mut busy = Vec::new();
    let mut renders = Vec::new();
    for event in events {
        match event {
            diag::Event::FreeCheck {
                at,
                free,
                holder_pid,
            } => {
                last_check = Some((at, free, holder_pid));
                if !free {
                    busy.push(super::probe::process_executable(holder_pid));
                }
            }
            diag::Event::Injecting { at, holder_pid } => injecting = Some((at, holder_pid)),
            diag::Event::Render {
                at,
                after_injection,
                rendered,
                reader_pid,
            } => renders.push(format!(
                "{}ms:{}:{}:{}",
                at.saturating_duration_since(started).as_millis(),
                if after_injection { "post" } else { "pre" },
                rendered,
                super::probe::process_executable(reader_pid)
            )),
        }
    }
    let boundary = injecting
        .map(|(at, pid)| {
            format!(
                "{}; final_free_check={:?}ms/{:?}",
                super::probe::process_executable(pid),
                last_check.map(|(check, _, _)| at.saturating_duration_since(check).as_millis()),
                last_check.map(|(_, free, pid)| (free, pid))
            )
        })
        .unwrap_or_else(|| "not injected".into());
    report.row(target, &format!("diagnostic {case}"), format!(
        "D1={}; holder_at_injection={boundary}; busy_holders={busy:?}; render_events={renders:?}; outcome={outcome:?}",
        diag::DECLINE.load(Ordering::SeqCst)
    ));
    // Synthesized formats appear lazily after a restore; let it settle.
    std::thread::sleep(Duration::from_secs(1));
    let after = clipboard_digest();
    if before.1 != after.1 {
        println!(
            "    clipboard before: {:?}
    clipboard after:  {:?}",
            before.1, after.1
        );
    }
    report.row(
        target,
        case,
        format!(
            "{outcome:?} in {} ms; clipboard seq {}->{}; formats+bytes preserved={} ({} formats)",
            elapsed.as_millis(),
            before.0,
            after.0,
            digest_preserved(&before.1, &before_again.1, &after.1),
            before.1.len()
        ),
    );
    outcome
}

/// Launches an isolated Edge app window on `page`, runs `body` with its
/// window, then closes it and every process using the probe's profile.
#[cfg(target_os = "windows")]
fn run_edge_page(
    report: &mut ProbeReport,
    tmp: &std::path::Path,
    label: &str,
    title: &str,
    page: &str,
    body: impl FnOnce(&mut ProbeReport, usize),
) {
    use super::probe::*;
    let html = tmp.join(format!("{}.html", title.replace(' ', "-")));
    std::fs::write(&html, page).unwrap();
    let edge = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe";
    let profile = tmp.join("edge-profile");
    let child = std::process::Command::new(edge)
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-sync",
        ])
        .arg(format!("--app={}", file_url(&html)))
        .spawn();
    let Ok(child) = child else {
        report.row(label, "launch", "msedge did not start");
        return;
    };
    let _cleanup = ProbeEdgeCleanup {
        child,
        profile: profile.clone(),
    };
    // msedge.exe relaunches itself, so match the page's unique title
    // instead of the launcher's pid.
    match wait_window(
        |w| w.class == "Chrome_WidgetWin_1" && w.title == title,
        Duration::from_secs(30),
    ) {
        Some(main) => {
            std::thread::sleep(Duration::from_secs(2));
            body(report, main.hwnd);
            close_window(main.hwnd);
        }
        None => report.row(label, "launch", "window did not appear"),
    }
    std::thread::sleep(Duration::from_secs(3));
    // Only processes using the probe's private profile.
}

#[cfg(target_os = "windows")]
struct ProbeEdgeCleanup {
    child: std::process::Child,
    profile: std::path::PathBuf,
}

#[cfg(target_os = "windows")]
impl Drop for ProbeEdgeCleanup {
    fn drop(&mut self) {
        super::probe::kill_processes_with_arg("msedge.exe", &self.profile.display().to_string());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Same format set, and equal bytes for every format whose bytes were stable
/// across two reads before the commit (synthesized formats are not).
#[cfg(target_os = "windows")]
fn digest_preserved(
    before: &[(u32, Option<u64>)],
    before_again: &[(u32, Option<u64>)],
    after: &[(u32, Option<u64>)],
) -> bool {
    let formats = |d: &[(u32, Option<u64>)]| d.iter().map(|f| f.0).collect::<Vec<_>>();
    formats(before) == formats(after)
        && before.iter().all(|(format, hash)| {
            let stable = before_again.iter().any(|(f, h)| f == format && h == hash);
            !stable || after.iter().any(|(f, h)| f == format && h == hash)
        })
}

/// Runs the case list against one launched target.
#[cfg(target_os = "windows")]
fn probe_one_target(
    report: &mut ProbeReport,
    uia: &super::probe::ProbeUia,
    target: &str,
    main: usize,
    other: usize,
    idle: Duration,
    do_commit: bool,
) {
    use super::probe::*;
    let settle = || std::thread::sleep(Duration::from_millis(400));
    if !force_foreground(main) {
        report.row(
            target,
            "foreground",
            "FAILED to bring probe window to front; skipped",
        );
        return;
    }
    report.row(target, "identity", uia.describe());

    // Cold: whatever the target shows at launch (no probe UIA reads of the
    // text yet).
    let seq0 = clipboard_digest().0;
    let (cold, elapsed) = capture(1);
    report.row(
        target,
        "cold capture (initial state)",
        format!(
            "{}; clipboard seq unchanged={}",
            describe_capture(&cold, elapsed),
            clipboard_digest().0 == seq0
        ),
    );
    drop(cold);

    // Multiline selection.
    let _ = uia.select_between(SEL_START, SEL_END);
    settle();
    let seq0 = clipboard_digest().0;
    let (multi, elapsed) = capture(2);
    let mut detail = format!(
        "{}; clipboard seq unchanged={}",
        describe_capture(&multi, elapsed),
        clipboard_digest().0 == seq0
    );
    if let Ok(Captured {
        text: CapturedText::Selected(s),
        ..
    }) = &multi
    {
        detail.push_str(&format!(
            "; exact markers={}; line break={:?}",
            s.starts_with(SEL_START) && s.ends_with(SEL_END),
            s.trim_start_matches(SEL_START).trim_end_matches(SEL_END)
        ));
    }
    report.row(target, "multiline selection", detail);

    if let Ok(captured) = multi {
        std::thread::sleep(idle);
        report.row(
            target,
            &format!("idle {} s", idle.as_secs()),
            describe_token(&captured.token),
        );
        let _ = uia.select_word("omega");
        settle();
        report.row(target, "selection changed", describe_token(&captured.token));
    }

    // ABA: change the selection, then restore the identical range.
    let _ = uia.select_between(SEL_START, SEL_END);
    settle();
    if let (Ok(c), _) = capture(3) {
        let _ = uia.select_word("alpha");
        settle();
        let _ = uia.select_between(SEL_START, SEL_END);
        settle();
        report.row(
            target,
            "selection changed and restored (ABA)",
            describe_token(&c.token),
        );
    }

    // Foreground round trip.
    if let (Ok(c), _) = capture(4) {
        let away = force_foreground(other);
        let back = force_foreground(main);
        settle();
        report.row(
            target,
            "alt-tab round trip",
            format!("away={away} back={back}; {}", describe_token(&c.token)),
        );
    }

    // Collapsed caret.
    let _ = uia.caret_before("omega");
    settle();
    let (caret, elapsed) = capture(5);
    report.row(target, "collapsed caret", describe_capture(&caret, elapsed));
    if let Ok(c) = caret {
        std::thread::sleep(Duration::from_secs(2));
        report.row(target, "collapsed caret idle 2 s", describe_token(&c.token));
    }

    if do_commit {
        // Stale target: selection changed after capture.
        let _ = uia.select_between(SEL_START, SEL_END);
        settle();
        if let (Ok(c), _) = capture(6) {
            let _ = uia.select_word("omega");
            settle();
            let outcome = probe_commit(
                report,
                target,
                "commit after selection change",
                c.token,
                "PROBE-STALE",
            );
            assert!(matches!(
                outcome,
                CommitOutcome::Refused {
                    reason: RefuseReason::TargetStale(_),
                    ..
                }
            ));
            report.row(
                target,
                "commit after selection change: document",
                format!("untouched={}", !uia.doc_text().contains("PROBE-STALE")),
            );
        }
        // Foreground change must refuse without typing into either window.
        uia.set_doc(DOC).unwrap();
        uia.select_between(SEL_START, SEL_END).unwrap();
        settle();
        let (captured, _) = capture(63);
        let captured = captured.unwrap();
        assert!(force_foreground(other));
        let outcome = probe_commit(
            report,
            target,
            "commit after focus switch",
            captured.token,
            "PROBE-STALE",
        );
        assert!(matches!(
            outcome,
            CommitOutcome::Refused {
                reason: RefuseReason::TargetStale(_),
                ..
            }
        ));
        assert!(force_foreground(main));
        assert_eq!(
            uia.doc_text().replace("\r\n", "\n"),
            DOC.replace("\r\n", "\n")
        );
        report.row(
            target,
            "commit after focus switch: document",
            "untouched=true; only probe windows focused",
        );

        // A collapsed cursor moving after capture is stale too.
        uia.caret_before("omega").unwrap();
        settle();
        let (captured, _) = capture(64);
        let captured = captured.unwrap();
        assert!(matches!(captured.text, CapturedText::Empty));
        uia.caret_before("alpha").unwrap();
        settle();
        let outcome = probe_commit(
            report,
            target,
            "commit after cursor change",
            captured.token,
            "PROBE-STALE",
        );
        assert!(matches!(
            outcome,
            CommitOutcome::Refused {
                reason: RefuseReason::TargetStale(_),
                ..
            }
        ));
        assert_eq!(
            uia.doc_text().replace("\r\n", "\n"),
            DOC.replace("\r\n", "\n")
        );
        report.row(
            target,
            "commit after cursor change: document",
            "untouched=true",
        );

        // Inject a genuine newer copy after production publication, using the
        // test hook at its existing platform seam. Run the real commit core.
        uia.select_between(SEL_START, SEL_END).unwrap();
        settle();
        let (captured, _) = capture(65);
        let captured = captured.unwrap();
        *crate::paste_tx::diag::AFTER_PUBLISH.lock().unwrap() = Some(Box::new(|| {
            set_clipboard(&[ClipItem::Bytes(13, utf16_bytes("PROBE-NEWER-COPY"))]).unwrap();
        }));
        let outcome = probe_commit(
            report,
            target,
            "newer copy after publication",
            captured.token,
            "PROBE-STALE",
        );
        assert_eq!(
            outcome,
            CommitOutcome::Refused {
                reason: RefuseReason::ClipboardChanged,
                clipboard: ClipboardSettlement::ExternallyChanged,
            }
        );
        let after_newer = clipboard_digest();
        let expected_newer_hash = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            utf16_bytes("PROBE-NEWER-COPY").hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(
            uia.doc_text().replace("\r\n", "\n"),
            DOC.replace("\r\n", "\n")
        );
        assert!(after_newer.1.contains(&(13, Some(expected_newer_hash))));
        report.row(
            target,
            "newer copy after publication: document",
            "untouched=true; newer copy persists",
        );
        // Real replacement and insertion at a verified empty caret,
        // repeated to count Delivered outcomes.
        let rounds: usize = std::env::var("HANDY_PROBE_COMMIT_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        for round in 1..=rounds {
            let _ = uia.set_doc(DOC);
            settle();
            let _ = uia.select_between(SEL_START, SEL_END);
            settle();
            if let (Ok(c), _) = capture(7) {
                probe_commit(
                    report,
                    target,
                    &format!("commit replacement #{round}"),
                    c.token,
                    "PROBE-REPLACEMENT",
                );
                let doc = uia.doc_text();
                report.row(
                    target,
                    &format!("commit replacement #{round}: document"),
                    format!(
                        "replaced={} markers gone={}",
                        doc.contains("alpha PROBE-REPLACEMENT omega"),
                        !doc.contains(SEL_START)
                    ),
                );
            }
            let _ = uia.caret_before("omega");
            settle();
            if let (Ok(c), _) = capture(8) {
                let empty = c.token.is_empty_target();
                probe_commit(
                    report,
                    target,
                    &format!("commit at empty caret #{round}"),
                    c.token,
                    "INSERTED ",
                );
                report.row(
                    target,
                    &format!("commit at empty caret #{round}: document"),
                    format!(
                        "empty token={empty}; inserted={}",
                        uia.doc_text().contains("INSERTED omega")
                    ),
                );
            }
        }
    }

    // Capture latency by selection size.
    for lines in [10usize, 50, 100, 200] {
        let doc: String = (0..lines)
            .map(|i| format!("{i:04} {}\r\n", "x".repeat(94)))
            .collect();
        if uia.set_doc(&doc).is_ok() {
            let last = format!("{:04} ", lines - 1);
            let _ = uia.select_between("0000 ", &last);
            settle();
            let (r, elapsed) = capture(40 + lines as u64);
            report.row(
                target,
                &format!("capture, ~{} chars", (lines - 1) * 100),
                describe_capture(&r, elapsed),
            );
        }
    }

    // Large selection (warm).
    let big: String = (0..200)
        .map(|i| format!("{i:04} {}\r\n", "x".repeat(94)))
        .collect();
    match uia.set_doc(&big) {
        Ok(()) => {
            // From the first to the last line marker: 19,904 characters.
            let _ = uia.select_between("0000 ", "0199 ");
            settle();
            let (large, elapsed) = capture(9);
            report.row(
                target,
                "20000-char selection",
                describe_capture(&large, elapsed),
            );
            // Repeat to separate first-touch cost from warm capture cost.
            let mut latest = large;
            for attempt in 2..=3 {
                std::thread::sleep(Duration::from_millis(1500));
                let (again, elapsed) = capture(9 + attempt);
                report.row(
                    target,
                    &format!("20000-char selection, capture {attempt}"),
                    describe_capture(&again, elapsed),
                );
                latest = again;
            }
            if let Ok(c) = latest {
                let started = Instant::now();
                let v = super::windows::probe_validate(c.token.id());
                report.row(
                    target,
                    "20000-char validate",
                    format!("{v:?} in {} ms", started.elapsed().as_millis()),
                );
            }
        }
        Err(e) => report.row(
            target,
            "20000-char selection",
            format!("could not set text: {e}"),
        ),
    }
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "manual live probe: launches and drives its own windows on this desktop"]
fn probe_live_target() {
    use super::probe::*;
    assert_eq!(
        std::env::var("HANDY_PROBE_DESKTOP_AVAILABLE").as_deref(),
        Ok("1"),
        "the live probe requires an announced idle desktop window"
    );
    let _windows = ProbeWindows;
    let mut session_clipboard = crate::paste_tx::ProbeClipboardSession::begin().unwrap();
    crate::paste_tx::diag::DECLINE.store(
        std::env::var("HANDY_PROBE_DECLINE").as_deref() != Ok("0"),
        Ordering::SeqCst,
    );
    let targets = std::env::var("HANDY_PROBE_TARGETS")
        .unwrap_or_else(|_| "edit,richedit,notepad,edge".into());
    let idle = Duration::from_secs(
        std::env::var("HANDY_PROBE_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10),
    );
    let do_commit = std::env::var("HANDY_PROBE_COMMIT").as_deref() == Ok("1");
    let uia = ProbeUia::new();
    let mut report = ProbeReport(Vec::new());
    let tmp = temp_dir();
    let (seq, formats) = clipboard_digest();
    report.row(
        "clipboard",
        "at start",
        format!(
            "seq {seq}; formats {:?}",
            formats.iter().map(|f| f.0).collect::<Vec<_>>()
        ),
    );

    // The classic Edit host also provides the "other" window for every target.
    let mut edit_host = spawn_edit_host("EDIT");
    let edit_pid = edit_host.id();
    let edit_main = wait_window(
        |w| w.pid == edit_pid && w.title == "HandyProbeHost EDIT",
        Duration::from_secs(10),
    );
    let edit_other = wait_window(
        |w| w.pid == edit_pid && w.title == "HandyProbeOther",
        Duration::from_secs(10),
    );
    let (Some(edit_main), Some(edit_other)) = (edit_main, edit_other) else {
        let _ = edit_host.kill();
        panic!("edit host windows did not appear");
    };

    for target in targets.split(',').map(str::trim) {
        match target {
            "edit" => {
                probe_one_target(
                    &mut report,
                    &uia,
                    "Win32 Edit",
                    edit_main.hwnd,
                    edit_other.hwnd,
                    idle,
                    do_commit,
                );
                if force_foreground(edit_other.hwnd) {
                    let (r, elapsed) = capture(20);
                    report.row(
                        "HandyProbeOther (plain window)",
                        "unsupported target",
                        describe_capture(&r, elapsed),
                    );
                }
                if do_commit && std::env::var("HANDY_PROBE_FORMATS").as_deref() == Ok("1") {
                    probe_clipboard_formats(&mut report, &uia, edit_main.hwnd);
                }
                if do_commit && std::env::var("HANDY_PROBE_ABORT_READER").as_deref() == Ok("1") {
                    assert!(force_foreground(edit_main.hwnd));
                    let result = super::probe::abort_reader_during_settlement().unwrap();
                    report.row(
                        "Win32 Edit",
                        "reader requests format during abort settlement",
                        result,
                    );
                }
            }
            "richedit" => {
                let mut host = spawn_edit_host("RICHEDIT50W");
                let pid = host.id();
                match wait_window(
                    |w| w.pid == pid && w.title == "HandyProbeHost RICHEDIT50W",
                    Duration::from_secs(10),
                ) {
                    Some(main) => probe_one_target(
                        &mut report,
                        &uia,
                        "RICHEDIT50W",
                        main.hwnd,
                        edit_other.hwnd,
                        idle,
                        do_commit,
                    ),
                    None => report.row("RICHEDIT50W", "launch", "window did not appear"),
                }
                close_window_of_pid(pid);
                wait_child(&mut host);
            }
            "notepad" => {
                if process_running("Notepad.exe") {
                    report.row(
                        "Notepad",
                        "launch",
                        "skipped: Notepad already running (a launch could open a tab in the user's window)",
                    );
                    continue;
                }
                let file = tmp.join("handy-probe.txt");
                std::fs::write(&file, DOC).unwrap();
                let _ = std::process::Command::new("notepad.exe").arg(&file).spawn();
                match wait_window(
                    |w| w.class == "Notepad" && w.title.contains("handy-probe"),
                    Duration::from_secs(15),
                ) {
                    Some(main) => {
                        std::thread::sleep(Duration::from_secs(1));
                        probe_one_target(
                            &mut report,
                            &uia,
                            "Notepad (Win11)",
                            main.hwnd,
                            edit_other.hwnd,
                            idle,
                            do_commit,
                        );
                        // Save to the temp file and close the tab so no
                        // unsaved session state is left behind.
                        if force_foreground(main.hwnd) {
                            ctrl_key_to(main.hwnd, 0x53);
                            std::thread::sleep(Duration::from_secs(1));
                            ctrl_key_to(main.hwnd, 0x57);
                        }
                        std::thread::sleep(Duration::from_secs(2));
                        if window_alive(main.hwnd) {
                            close_window(main.hwnd);
                            std::thread::sleep(Duration::from_secs(2));
                        }
                        report.row(
                            "Notepad (Win11)",
                            "cleanup",
                            format!("window closed={}", !window_alive(main.hwnd)),
                        );
                    }
                    None => report.row("Notepad (Win11)", "launch", "window did not appear"),
                }
            }
            "edge" => {
                let start = DOC.find(SEL_START).unwrap();
                let end = start + SEL_START.len() + 1 + SEL_END.len();
                let page = format!(
                    concat!(
                        "<!doctype html><title>HandyProbe Edge</title>",
                        "<textarea id=t rows=12 cols=70 autofocus></textarea>",
                        "<script>const t=document.getElementById('t');",
                        "t.value={:?};t.focus();t.setSelectionRange({},{});</script>"
                    ),
                    DOC.replace("\r\n", "\n"),
                    start,
                    end
                );
                run_edge_page(
                    &mut report,
                    &tmp,
                    "Edge textarea",
                    "HandyProbe Edge",
                    &page,
                    |report, main| {
                        probe_one_target(
                            report,
                            &uia,
                            "Edge textarea",
                            main,
                            edit_other.hwnd,
                            idle,
                            do_commit,
                        );
                    },
                );
            }
            "edge-ce" => {
                // contenteditable root with a non-editable island; the page
                // selects from CE-START across the island to CE-END.
                let page = concat!(
                    "<!doctype html><title>HandyProbe EdgeCE</title>",
                    "<div id=ce contenteditable role=textbox style='border:1px solid;min-height:5em'>",
                    "alpha CE-START <span contenteditable=false>LOCKED</span> CE-END omega</div>",
                    "<script>const ce=document.getElementById('ce');ce.focus();",
                    "const r=document.createRange();r.setStart(ce.firstChild,6);",
                    "r.setEnd(ce.lastChild,7);const s=getSelection();s.removeAllRanges();",
                    "s.addRange(r);</script>"
                );
                run_edge_page(
                    &mut report,
                    &tmp,
                    "Edge contenteditable",
                    "HandyProbe EdgeCE",
                    page,
                    |report, main| {
                        let target = "Edge contenteditable";
                        if !force_foreground(main) {
                            report.row(target, "foreground", "FAILED");
                            return;
                        }
                        report.row(target, "identity", uia.describe());
                        let (r, elapsed) = capture(60);
                        report.row(
                            target,
                            "selection across contenteditable=false island",
                            describe_capture(&r, elapsed),
                        );
                        drop(r);
                        let _ = uia.select_word("alpha");
                        std::thread::sleep(Duration::from_millis(400));
                        let (r, elapsed) = capture(61);
                        report.row(
                            target,
                            "selection of editable word (control)",
                            describe_capture(&r, elapsed),
                        );
                        report.row(
                            target,
                            "provider read-only evidence",
                            uia.read_only_report("LOCKED", "CE-START", "CE-END"),
                        );
                        let _ = uia.select_between("CE-START", "CE-END");
                        std::thread::sleep(Duration::from_millis(400));
                        if let (Ok(c), _) = capture(62) {
                            probe_commit(
                                report,
                                target,
                                "commit over the island",
                                c.token,
                                "REPLACED",
                            );
                            report.row(
                                target,
                                "commit over the island: document",
                                uia.doc_text().replace('\n', " "),
                            );
                        }
                    },
                );
            }
            other => report.row(other, "launch", "unknown target"),
        }
    }

    close_window(edit_main.hwnd);
    wait_child(&mut edit_host);
    let started = Instant::now();
    shutdown();
    report.row(
        "worker",
        "shutdown",
        format!("returned in {} ms", started.elapsed().as_millis()),
    );
    let (seq, formats) = clipboard_digest();
    report.row(
        "clipboard",
        "at end",
        format!(
            "seq {seq}; formats {:?}",
            formats.iter().map(|f| f.0).collect::<Vec<_>>()
        ),
    );
    let restored = session_clipboard.restore();
    report.row(
        "clipboard",
        "session backup restored",
        format!("{restored:?}"),
    );
    assert!(matches!(
        restored,
        ClipboardSettlement::Restored | ClipboardSettlement::ExternallyChanged
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    if let Ok(path) = std::env::var("HANDY_PROBE_REPORT") {
        std::fs::write(path, report.0.join("\n")).unwrap();
    }
    println!("\nPROBE REPORT\n{}", report.0.join("\n"));
}

/// Commits into the Win32 Edit host with known clipboard contents. Only run
/// under a session-wide lossless backup; fixtures are guarded against an
/// external clipboard copy, and the session restores the user's formats.
#[cfg(target_os = "windows")]
fn probe_clipboard_formats(report: &mut ProbeReport, uia: &super::probe::ProbeUia, main: usize) {
    use super::probe::*;
    let html = registered_format("HTML Format");
    let large = registered_format("HandyProbeLarge");
    let text = || ClipItem::Bytes(13, utf16_bytes("PROBE-CLIP-TEXT"));
    let cases: Vec<(&str, Vec<ClipItem>)> = vec![
        ("text", vec![text()]),
        ("image (CF_DIB)", vec![ClipItem::Bytes(8, tiny_dib())]),
        (
            "multi-format (text+HTML+DIB)",
            vec![
                text(),
                ClipItem::Bytes(html, b"Version:0.9\r\n<b>probe</b>\0".to_vec()),
                ClipItem::Bytes(8, tiny_dib()),
            ],
        ),
        ("EMF (Office-style)", vec![text(), ClipItem::EnhMetafile]),
        (
            "65 MiB registered format",
            vec![text(), ClipItem::Bytes(large, vec![7u8; 65 * 1024 * 1024])],
        ),
    ];
    for (i, (name, items)) in cases.iter().enumerate() {
        let target = format!("clipboard: {name}");
        if let Err(e) = set_clipboard(items) {
            report.row(&target, "fixture", format!("could not set clipboard: {e}"));
            continue;
        }
        // Let clipboard listeners react to the fixture before committing.
        std::thread::sleep(Duration::from_secs(1));
        if !force_foreground(main) {
            report.row(&target, "foreground", "FAILED");
            continue;
        }
        let _ = uia.set_doc(DOC);
        let _ = uia.caret_before("omega");
        std::thread::sleep(Duration::from_millis(400));
        match capture(30 + i as u64) {
            (Ok(c), _) => {
                let marker = format!("CLIP{i} ");
                probe_commit(report, &target, "commit into Win32 Edit", c.token, &marker);
                report.row(
                    &target,
                    "document",
                    format!(
                        "inserted={}",
                        uia.doc_text().contains(&format!("{marker}omega"))
                    ),
                );
            }
            (Err(e), _) => report.row(&target, "capture", format!("{e:?}")),
        }
    }
    let emptied = set_clipboard(&[]);
    report.row("clipboard", "fixtures removed", format!("{emptied:?}"));
}
