use super::*;
use crate::llm_client::test_http::{completion, FakeServer, Step};
use crate::selection::{
    CapturedText, FailReason, InvalidationReason, SessionId, StaleReason, TargetId, TargetToken,
    UncertainReason,
};
use std::sync::atomic::{AtomicU8, AtomicUsize};

static TRACKER: CommitTracker = CommitTracker::new();

pub(crate) fn configured_settings(url: &str) -> AppSettings {
    let mut settings = crate::settings::get_default_settings();
    settings.post_process_provider_id = "custom".into();
    settings.post_process_providers = vec![crate::llm_client::test_http::provider(url)];
    settings
        .post_process_models
        .insert("custom".into(), "chosen-model".into());
    settings
}
pub(crate) fn session(
    url: &str,
    captured: CapturedText,
    store: &Arc<RecoveryStore>,
) -> (AssistantSession, Arc<AtomicUsize>) {
    let settings = configured_settings(url);
    let admission = admit_binding(false, &settings, "assistant", store)
        .unwrap_or_else(|_| panic!("valid admission"));
    let released = Arc::new(AtomicUsize::new(0));
    let probe = Arc::clone(&released);
    let token = TargetToken::new(
        TargetId(7),
        SessionId(42),
        matches!(captured, CapturedText::Empty),
        Arc::new(AtomicU8::new(0)),
        Arc::new(move |_| {
            probe.fetch_add(1, Ordering::SeqCst);
        }),
    );
    (
        AssistantSession {
            id: SessionId(42),
            request: admission.request,
            captured,
            token,
            reservation: admission.reservation,
            clipboard_handling: admission.clipboard_handling,
            capture: Duration::from_millis(9),
            cancel_arm: CancelArm::noop(),
        },
        released,
    )
}
pub(crate) fn never_cancel() -> CancelProbe {
    Arc::new(|| false)
}
pub(crate) fn successful_server_step(text: &str) -> Step {
    Step::Respond(
        200,
        completion(
            Some(&serde_json::json!({"replacement": text}).to_string()),
            "stop",
        ),
    )
}
fn item(id: u64) -> AssistantRecoveryItem {
    AssistantRecoveryItem {
        id,
        created_at: 0,
        target: TargetKind::Selected,
        outcome: RecoveryOutcome::Refused,
        reason: "target_stale".into(),
        detail: Some("focus_changed".into()),
        clipboard: "untouched".into(),
        instruction: "private instruction".into(),
        generated: "private generated".into(),
    }
}
fn delivered() -> CommitOutcome {
    CommitOutcome::Delivered {
        clipboard: ClipboardSettlement::Restored,
    }
}

#[test]
fn recovery_reserve_fill_dismiss_round_trip() {
    let store = RecoveryStore::new(RECOVERY_CAPACITY);
    let reservation = store.reserve().unwrap();
    assert_eq!(store.free_slots(), 7);
    assert!(store.items().is_empty());
    let item = reservation.fill(item);
    assert_eq!(store.get(item.id).unwrap().generated, "private generated");
    assert!(store.dismiss(item.id));
    assert!(!store.dismiss(item.id));
    assert_eq!(store.free_slots(), 8);
}
#[test]
fn recovery_capacity_is_eight_and_ninth_reservation_fails() {
    let store = RecoveryStore::new(RECOVERY_CAPACITY);
    let reserved: Vec<_> = (0..8).map(|_| store.reserve().unwrap()).collect();
    assert!(store.reserve().is_none());
    assert_eq!(store.free_slots(), 0);
    drop(reserved);
    assert_eq!(store.free_slots(), 8);
}
#[test]
fn recovery_full_never_evicts_and_dismiss_frees_one_slot() {
    let store = RecoveryStore::new(8);
    let ids: Vec<_> = (0..8)
        .map(|_| store.reserve().unwrap().fill(item).id)
        .collect();
    assert!(store.reserve().is_none());
    assert_eq!(store.items().iter().map(|i| i.id).collect::<Vec<_>>(), ids);
    assert!(store.dismiss(ids[3]));
    let next = store.reserve().unwrap().fill(item);
    assert!(next.id > ids[7]);
    assert_eq!(store.free_slots(), 0);
    assert!(store.get(ids[0]).is_some());
}
#[test]
fn recovery_dropped_reservation_releases_slot() {
    let store = RecoveryStore::new(8);
    drop(store.reserve().unwrap());
    assert_eq!(store.free_slots(), 8);
}
#[test]
fn recovery_item_debug_redacts_text() {
    let debug = format!("{:?}", item(1));
    assert!(debug.contains("target_stale"));
    assert!(!debug.contains("private instruction"));
    assert!(!debug.contains("private generated"));
}
#[test]
fn new_store_is_empty_no_replay() {
    let old = RecoveryStore::new(8);
    old.reserve().unwrap().fill(item);
    drop(old);
    let new = RecoveryStore::new(8);
    assert!(new.items().is_empty());
    assert_eq!(new.free_slots(), 8);
}
#[test]
fn admission_rejects_shutdown_without_reserving() {
    let store = RecoveryStore::new(8);
    assert!(matches!(
        admit_binding(
            true,
            &configured_settings("http://unused.invalid"),
            "assistant",
            &store
        ),
        Err(IntakeRejection::ShuttingDown)
    ));
    assert_eq!(store.free_slots(), 8);
}
#[test]
fn admission_rejects_missing_provider_model_or_prompt_before_capture() {
    for missing in ["provider", "model", "prompt"] {
        let mut settings = configured_settings("http://unused.invalid");
        match missing {
            "provider" => settings.post_process_provider_id.clear(),
            "model" => settings.post_process_models.clear(),
            _ => settings.assistant_selected_prompt_id = None,
        }
        let store = RecoveryStore::new(8);
        assert!(
            matches!(
                admit_binding(false, &settings, "assistant", &store),
                Err(IntakeRejection::Config(_))
            ),
            "{missing}"
        );
        assert_eq!(store.free_slots(), 8);
    }
}
#[test]
fn admission_rejects_full_capacity() {
    let store = RecoveryStore::new(8);
    let reserved: Vec<_> = (0..8).map(|_| store.reserve().unwrap()).collect();
    assert!(matches!(
        admit_binding(
            false,
            &configured_settings("http://unused.invalid"),
            "assistant",
            &store
        ),
        Err(IntakeRejection::RecoveryFull)
    ));
    assert_eq!(store.free_slots(), 0);
    drop(reserved);
}
#[test]
fn admission_snapshots_request_and_clipboard_policy() {
    let mut settings = configured_settings("http://unused.invalid");
    let store = RecoveryStore::new(8);
    let admitted =
        admit_binding(false, &settings, "assistant", &store).unwrap_or_else(|_| panic!());
    settings.post_process_models.clear();
    settings.assistant_selected_prompt_id = None;
    settings.clipboard_handling = ClipboardHandling::CopyToClipboard;
    assert_eq!(admitted.request.target.model, "chosen-model");
    assert!(!admitted.request.prompt.is_empty());
    assert_eq!(admitted.clipboard_handling, ClipboardHandling::DontModify);
}

async fn replacement_case(captured: CapturedText) {
    let server = FakeServer::new(vec![successful_server_step("Generated replacement.")]).await;
    let store = RecoveryStore::new(8);
    let empty = matches!(captured, CapturedText::Empty);
    let expected_source = match &captured {
        CapturedText::Selected(text) => text.clone(),
        CapturedText::Empty => "".into(),
    };
    let (session, released) = session(&server.url, captured, &store);
    let report = complete_session(
        session,
        "private spoken instruction",
        never_cancel(),
        move |req| {
            assert_eq!(req.replacement, "Generated replacement.");
            assert_ne!(req.replacement, "private spoken instruction");
            assert_eq!(req.token.is_empty_target(), empty);
            assert_eq!(req.session, SessionId(42));
            delivered()
        },
        &TRACKER,
    )
    .await;
    assert!(matches!(report.outcome, SessionOutcome::Delivered));
    assert_eq!(report.commit_calls, 1);
    assert_eq!(
        report.selection_scalars,
        expected_source.chars().count() as u64
    );
    assert_eq!(store.free_slots(), 8);
    assert_eq!(released.load(Ordering::SeqCst), 1);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let user: serde_json::Value =
        serde_json::from_str(requests[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(user["selected_text"], expected_source);
}
#[tokio::test]
async fn selected_replacement_commits_generated_text_once() {
    replacement_case(CapturedText::Selected("original sentence".into())).await;
}
#[tokio::test]
async fn empty_target_inserts_generated_text() {
    replacement_case(CapturedText::Empty).await;
}
#[tokio::test]
async fn whitespace_selection_is_sent_and_replaced_verbatim() {
    replacement_case(CapturedText::Selected(" \t\n".into())).await;
}
#[tokio::test]
async fn blank_instruction_skips_llm_and_commit() {
    let server = FakeServer::new(vec![]).await;
    let store = RecoveryStore::new(8);
    let (session, released) = session(&server.url, CapturedText::Empty, &store);
    let report = complete_session(
        session,
        " \n",
        never_cancel(),
        |_| panic!("no commit"),
        &TRACKER,
    )
    .await;
    assert!(matches!(
        report.outcome,
        SessionOutcome::Failed(AssistantError {
            code: "blank_instruction",
            ..
        })
    ));
    assert_eq!(report.commit_calls, 0);
    assert_eq!(server.accepts.load(Ordering::SeqCst), 0);
    assert_eq!(released.load(Ordering::SeqCst), 1);
    assert_eq!(store.free_slots(), 8);
}
#[tokio::test]
async fn llm_provider_failure_never_commits_and_keeps_no_item() {
    let server = FakeServer::new(vec![Step::Respond(503, "private failure".into())]).await;
    let store = RecoveryStore::new(8);
    let (session, released) = session(
        &server.url,
        CapturedText::Selected("original".into()),
        &store,
    );
    let report = complete_session(
        session,
        "instruction",
        never_cancel(),
        |_| panic!("no mutation"),
        &TRACKER,
    )
    .await;
    assert!(matches!(
        report.outcome,
        SessionOutcome::Failed(AssistantError {
            code: "llm_provider",
            reason: Some("provider"),
            ..
        })
    ));
    assert_eq!(report.commit_calls, 0);
    assert!(store.items().is_empty());
    assert_eq!(store.free_slots(), 8);
    assert_eq!(released.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn context_exceeded_preserves_selection_and_keeps_no_recovery_item() {
    let server = FakeServer::new(vec![Step::Respond(400,
        r#"{"error":{"code":400,"message":"request (18012 tokens) exceeds the available context size (16384 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":18012,"n_ctx":16384}}"#.into())]).await;
    let store = RecoveryStore::new(8);
    let (session, released) = session(
        &server.url,
        CapturedText::Selected("original selection".into()),
        &store,
    );
    let report = complete_session(
        session,
        "instruction",
        never_cancel(),
        |_| panic!("selection must stay untouched"),
        &TRACKER,
    )
    .await;
    assert!(matches!(
        report.outcome,
        SessionOutcome::Failed(AssistantError {
            code: "llm_context_exceeded",
            reason: Some("context_exceeded"),
            ..
        })
    ));
    assert_eq!(report.commit_calls, 0);
    assert!(store.items().is_empty());
    assert_eq!(store.free_slots(), 8);
    assert_eq!(released.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn llm_invalid_or_reasoning_only_output_never_commits() {
    for text in [
        "not json",
        "<think>private reasoning</think>",
        r#"{"replacement":""}"#,
    ] {
        let server =
            FakeServer::new(vec![Step::Respond(200, completion(Some(text), "stop"))]).await;
        let store = RecoveryStore::new(8);
        let (session, _) = session(&server.url, CapturedText::Empty, &store);
        let report = complete_session(
            session,
            "instruction",
            never_cancel(),
            |_| panic!("no mutation"),
            &TRACKER,
        )
        .await;
        assert!(matches!(
            report.outcome,
            SessionOutcome::Failed(AssistantError {
                code: "llm_invalid_output",
                ..
            })
        ));
        assert_eq!(report.commit_calls, 0);
        assert_eq!(store.free_slots(), 8);
    }
}
#[tokio::test]
async fn llm_cancel_drops_request_without_commit() {
    let server = FakeServer::new(vec![Step::StallBeforeHeaders]).await;
    let store = RecoveryStore::new(8);
    let (session, released) = session(&server.url, CapturedText::Empty, &store);
    let cancel = Arc::new(AtomicBool::new(false));
    let probe = Arc::clone(&cancel);
    let task = tokio::spawn(complete_session(
        session,
        "instruction",
        Arc::new(move || probe.load(Ordering::SeqCst)),
        |_| panic!("no commit"),
        &TRACKER,
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancel.store(true, Ordering::SeqCst);
    let report = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(report.outcome, SessionOutcome::Discarded));
    assert_eq!(report.commit_calls, 0);
    assert_eq!(store.free_slots(), 8);
    assert_eq!(released.load(Ordering::SeqCst), 1);
}

async fn recoverable_case(
    outcome: CommitOutcome,
    expected: (&str, Option<&str>, &str),
) -> (Arc<RecoveryStore>, u64) {
    let server = FakeServer::new(vec![successful_server_step("generated")]).await;
    let store = RecoveryStore::new(8);
    let (session, released) = session(
        &server.url,
        CapturedText::Selected("private source".into()),
        &store,
    );
    let report = complete_session(
        session,
        "instruction",
        never_cancel(),
        |req| {
            assert_eq!(req.replacement, "generated");
            outcome
        },
        &TRACKER,
    )
    .await;
    let SessionOutcome::Recoverable(item) = report.outcome else {
        panic!("recovery required")
    };
    assert_eq!(item.reason, expected.0);
    assert_eq!(item.detail.as_deref(), expected.1);
    assert_eq!(item.clipboard, expected.2);
    assert_eq!(item.instruction, "instruction");
    assert_eq!(item.generated, "generated");
    assert!(!format!("{item:?}").contains("private source"));
    assert_eq!(store.free_slots(), 7);
    assert_eq!(report.commit_calls, 1);
    assert_eq!(released.load(Ordering::SeqCst), 1);
    (store, item.id)
}
#[tokio::test]
async fn stale_focus_refusal_keeps_output_for_recovery() {
    recoverable_case(
        CommitOutcome::Refused {
            reason: RefuseReason::TargetStale(StaleReason::FocusChanged),
            clipboard: ClipboardSettlement::Untouched,
        },
        ("target_stale", Some("focus_changed"), "untouched"),
    )
    .await;
}
#[tokio::test]
async fn stale_range_refusal_is_recoverable() {
    for (reason, detail) in [
        (StaleReason::SelectionChanged, "selection_changed"),
        (
            StaleReason::Invalidated(InvalidationReason::TextSelection),
            "text_selection",
        ),
    ] {
        recoverable_case(
            CommitOutcome::Refused {
                reason: RefuseReason::TargetStale(reason),
                clipboard: ClipboardSettlement::Restored,
            },
            ("target_stale", Some(detail), "restored"),
        )
        .await;
    }
}
#[tokio::test]
async fn failed_injection_is_recoverable() {
    recoverable_case(
        CommitOutcome::Failed {
            reason: FailReason::InjectionNotSent("OS private detail".into()),
            clipboard: ClipboardSettlement::Restored,
        },
        ("injection_not_sent", None, "restored"),
    )
    .await;
}
#[tokio::test]
async fn uncertain_delivery_is_recoverable_and_never_retried() {
    recoverable_case(
        CommitOutcome::Uncertain {
            reason: UncertainReason::PartialInjection("OS private detail".into()),
            clipboard: ClipboardSettlement::ExternallyChanged,
        },
        ("partial_injection", None, "externally_changed"),
    )
    .await;
}
#[tokio::test]
async fn recovery_item_outlives_session_and_completion() {
    let (store, id) = recoverable_case(
        CommitOutcome::Uncertain {
            reason: UncertainReason::NoReceipt,
            clipboard: ClipboardSettlement::Restored,
        },
        ("no_receipt", None, "restored"),
    )
    .await;
    assert_eq!(store.get(id).unwrap().generated, "generated");
    assert!(store.dismiss(id));
    assert_eq!(store.free_slots(), 8);
}
#[tokio::test]
async fn cancelled_or_shutdown_commit_discards_output() {
    for reason in [RefuseReason::Cancelled, RefuseReason::Shutdown] {
        let server = FakeServer::new(vec![successful_server_step("generated")]).await;
        let store = RecoveryStore::new(8);
        let (session, released) = session(&server.url, CapturedText::Empty, &store);
        let report = complete_session(
            session,
            "instruction",
            never_cancel(),
            move |_| CommitOutcome::Refused {
                reason,
                clipboard: ClipboardSettlement::Restored,
            },
            &TRACKER,
        )
        .await;
        assert!(matches!(report.outcome, SessionOutcome::Discarded));
        assert_eq!(report.commit_calls, 1);
        assert!(store.items().is_empty());
        assert_eq!(store.free_slots(), 8);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn commit_receives_session_id_and_snapshotted_policy() {
    let server = FakeServer::new(vec![successful_server_step("generated")]).await;
    let store = RecoveryStore::new(8);
    let (mut session, _) = session(&server.url, CapturedText::Empty, &store);
    session.clipboard_handling = ClipboardHandling::CopyToClipboard;
    let report = complete_session(
        session,
        "instruction",
        never_cancel(),
        |req| {
            assert_eq!(req.session, SessionId(42));
            assert_eq!(req.clipboard_handling, ClipboardHandling::CopyToClipboard);
            assert!(!(req.is_cancelled)());
            delivered()
        },
        &TRACKER,
    )
    .await;
    assert!(matches!(report.outcome, SessionOutcome::Delivered));
}
#[tokio::test]
async fn commit_panic_is_uncertain_recovery() {
    let server = FakeServer::new(vec![successful_server_step("generated")]).await;
    let store = RecoveryStore::new(8);
    let (session, released) = session(&server.url, CapturedText::Empty, &store);
    let report = complete_session(
        session,
        "instruction",
        never_cancel(),
        |_| panic!("simulated commit crash"),
        &TRACKER,
    )
    .await;
    let SessionOutcome::Recoverable(item) = report.outcome else {
        panic!("uncertain recovery required")
    };
    assert_eq!(item.reason, "settlement_unacknowledged");
    assert_eq!(item.clipboard, "unknown");
    assert_eq!(report.commit_calls, 1);
    assert_eq!(released.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn completion_waits_for_commit_acknowledgment() {
    static OWN_TRACKER: CommitTracker = CommitTracker::new();
    let server = FakeServer::new(vec![successful_server_step("generated")]).await;
    let store = RecoveryStore::new(8);
    let (session, _) = session(&server.url, CapturedText::Empty, &store);
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let mut task = tokio::spawn(complete_session(
        session,
        "instruction",
        never_cancel(),
        move |_| {
            entered.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(3)).unwrap();
            delivered()
        },
        &OWN_TRACKER,
    ));
    ready.await.unwrap();
    assert!(!OWN_TRACKER.wait_idle(Duration::ZERO));
    assert!(tokio::time::timeout(Duration::from_millis(50), &mut task)
        .await
        .is_err());
    assert_eq!(store.free_slots(), 7);
    release.send(()).unwrap();
    assert!(matches!(
        task.await.unwrap().outcome,
        SessionOutcome::Delivered
    ));
    assert!(OWN_TRACKER.wait_idle(Duration::ZERO));
    assert_eq!(store.free_slots(), 8);
}
#[tokio::test]
async fn cancel_arm_disarms_once_before_completion_returns() {
    let server = FakeServer::new(vec![successful_server_step("generated")]).await;
    let store = RecoveryStore::new(8);
    let (mut session, _) = session(&server.url, CapturedText::Empty, &store);
    let disarmed = Arc::new(AtomicUsize::new(0));
    let on_drop = Arc::clone(&disarmed);
    session.cancel_arm = CancelArm(Some(Box::new(move || {
        on_drop.fetch_add(1, Ordering::SeqCst);
    })));
    let during_commit = Arc::clone(&disarmed);
    let report = complete_session(
        session,
        "instruction",
        never_cancel(),
        move |_| {
            assert_eq!(during_commit.load(Ordering::SeqCst), 0);
            delivered()
        },
        &TRACKER,
    )
    .await;
    assert!(matches!(report.outcome, SessionOutcome::Delivered));
    assert_eq!(disarmed.load(Ordering::SeqCst), 1);
    drop(report);
    assert_eq!(disarmed.load(Ordering::SeqCst), 1);
}
#[test]
fn commit_tracker_wait_idle_waits_for_in_flight_commit() {
    static OWN_TRACKER: CommitTracker = CommitTracker::new();
    let guard = OWN_TRACKER.enter();
    let (entered, wait) = std::sync::mpsc::channel();
    let task = std::thread::spawn(move || {
        entered.send(()).unwrap();
        OWN_TRACKER.wait_idle(Duration::from_secs(2))
    });
    wait.recv().unwrap();
    assert!(!OWN_TRACKER.wait_idle(Duration::ZERO));
    drop(guard);
    assert!(task.join().unwrap());
}
#[test]
fn commit_tracker_wait_idle_is_bounded() {
    static OWN_TRACKER: CommitTracker = CommitTracker::new();
    let _guard = OWN_TRACKER.enter();
    let start = Instant::now();
    assert!(!OWN_TRACKER.wait_idle(Duration::from_millis(5)));
    assert!(start.elapsed() < Duration::from_secs(1));
}
#[test]
fn codes_are_stable() {
    for (value, code) in [
        (ClipboardSettlement::Untouched, "untouched"),
        (ClipboardSettlement::Restored, "restored"),
        (ClipboardSettlement::LeftReplacement, "left_replacement"),
        (ClipboardSettlement::ExternallyChanged, "externally_changed"),
        (
            ClipboardSettlement::RestoreFailed("private".into()),
            "restore_failed",
        ),
        (ClipboardSettlement::Unknown, "unknown"),
    ] {
        assert_eq!(settlement_code(&value), code);
    }
    assert_eq!(
        LlmError::Config(crate::llm_client::ConfigError::NoPrompt).reason(),
        "no_prompt"
    );
    assert_eq!(
        LlmError::InvalidOutput(crate::llm_client::OutputError::ReasoningOnly).reason(),
        "reasoning_only"
    );
}
