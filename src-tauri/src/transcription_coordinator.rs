use crate::actions::{DictationSession, TranscribeAction, WorkflowMode};
use crate::managers::audio::AudioRecordingManager;
use crate::settings::{get_settings, ShortcutActivation};
use log::{debug, error, warn};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

const DEBOUNCE: Duration = Duration::from_millis(30);
const RELEASE_GRACE: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PttAction {
    Passthrough,
    DeferRelease,
    CancelRelease,
}

/// A key-up deferred by `RELEASE_GRACE` so a synthesized X11 auto-repeat
/// press can cancel it (#1539). When the grace elapses the hold is resolved
/// by [`CoordinatorState::finish_hold`] (recording) or
/// [`CoordinatorState::finish_pending_hold`] (press remembered while busy).
struct PendingRelease {
    binding_id: String,
    hotkey_string: String,
    deadline: Instant,
    /// When the key actually went up. The hold duration is measured to this
    /// instant, not to the grace expiry.
    released_at: Instant,
    /// Holds at least this long stop recording; shorter ones lock it on.
    /// Push-to-talk passes zero so every release stops.
    hold_threshold: Duration,
}

/// A press that arrived while the pipeline was still busy processing the
/// previous transcription. Toggle-style triggers (SIGUSR2, CLI flags, some
/// pedal setups) flip state on every edge, so dropping a busy press desyncs
/// the parity: the next edge starts a recording nobody will ever stop.
struct PendingPress {
    binding_id: String,
    hotkey_string: String,
    /// The real key-down time, so a hold that straddles the drain is still
    /// measured from when the user pressed, not from when recording began.
    pressed_at: Instant,
    /// The recording will start locked on when the pipeline drains: set from
    /// the start for toggle, and for hold-or-toggle once the key came back up
    /// within the threshold (a tap). An unlocked pending press is a key we
    /// believe is still held.
    locked: bool,
}

impl PendingPress {
    fn remembered(&self) -> Remembered {
        if self.locked {
            Remembered::Locked
        } else {
            Remembered::Held
        }
    }
}

/// What kind of press is already waiting for the pipeline to drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Remembered {
    /// The key is still down as far as we know.
    Held,
    /// A toggle press or a classified tap: it will start a locked session.
    Locked,
}

/// Bookkeeping for the key press that started the current recording.
struct Hold {
    pressed_at: Instant,
    /// Recording outlives the key: the next press stops it, releases are
    /// ignored. Always set for toggle; set for hold-or-toggle once a release
    /// has been classified as a tap.
    locked: bool,
}

/// What to do with an input that arrives while the pipeline is busy
/// (`Stage::Processing`). `remembered` is the press for the same binding
/// already waiting for the pipeline to drain, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusyAction {
    /// Ignore the input entirely.
    Ignore,
    /// Remember the press; start recording when the pipeline finishes.
    Remember,
    /// This press cancels a previously remembered press: two presses during
    /// one busy window net to no-op, exactly as a press stops a locked
    /// session once recording.
    Forget,
}

fn classify_busy_input(
    is_pressed: bool,
    mode: ShortcutActivation,
    remembered: Option<Remembered>,
) -> BusyAction {
    use ShortcutActivation::*;
    match (mode, is_pressed, remembered) {
        // Toggle: presses alternate remember/forget to preserve parity.
        (Toggle, true, Some(_)) => BusyAction::Forget,
        (Toggle, true, None) => BusyAction::Remember,
        // Toggle mode ignores releases.
        (Toggle, false, _) => BusyAction::Ignore,
        // Hold modes: a press while busy means the user is holding the key —
        // start as soon as the pipeline drains. A press on a queued tap stops
        // it (parity); a press while the key is already down is a repeat.
        (PushToTalk | HoldOrToggle, true, None) => BusyAction::Remember,
        (PushToTalk | HoldOrToggle, true, Some(Remembered::Locked)) => BusyAction::Forget,
        (PushToTalk | HoldOrToggle, true, Some(Remembered::Held)) => BusyAction::Ignore,
        // Releases of a held pending press are deferred by the grace window
        // before reaching here and resolved by `finish_pending_hold`; any
        // other release (no press remembered, or already locked) is noise.
        (PushToTalk | HoldOrToggle, false, _) => BusyAction::Ignore,
    }
}

/// Pipeline lifecycle.
#[derive(Debug, PartialEq, Eq)]
enum Stage {
    Idle,
    Capturing(String),
    Recording(String), // binding_id
    Processing,
}

/// A keyboard/signal edge for a transcribe binding.
struct InputEvent {
    binding_id: String,
    hotkey_string: String,
    is_pressed: bool,
    mode: ShortcutActivation,
    /// Hold-or-toggle: minimum press duration that counts as a hold.
    hold_threshold: Duration,
    /// External triggers (SIGUSR2, CLI flags) rather than physical keys.
    /// They fire on every edge by design and must never be debounced —
    /// dropping one desyncs toggle parity and wedges recording on.
    external: bool,
}

impl InputEvent {
    /// The hold duration at or above which a release stops recording.
    fn effective_hold_threshold(&self) -> Duration {
        match self.mode {
            ShortcutActivation::HoldOrToggle => self.hold_threshold,
            // Every release stops; toggle never defers releases at all.
            ShortcutActivation::PushToTalk | ShortcutActivation::Toggle => Duration::ZERO,
        }
    }
}

/// A side effect decided by [`CoordinatorState`]; the coordinator thread is
/// the only executor. Keeping decisions pure lets tests drive the exact
/// production transitions without a Tauri `AppHandle` or real timers.
#[derive(Debug, PartialEq, Eq)]
enum Effect {
    BeginCapture {
        binding_id: String,
        hotkey_string: String,
        session: u64,
    },
    CancelCapture {
        session: u64,
    },
    StartAssistant {
        binding_id: String,
        hotkey_string: String,
        session: u64,
    },
    RejectAssistant,
    Start {
        binding_id: String,
        hotkey_string: String,
    },
    Stop {
        binding_id: String,
        hotkey_string: String,
    },
}

/// Commands processed sequentially by the coordinator thread.
enum Command {
    Input(InputEvent),
    Cancel,
    ProcessingFinished,
    CaptureDone {
        session: u64,
        result: Result<crate::selection::Captured, crate::selection::CaptureError>,
    },
}

/// Explicit cancels shared by callers and the coordinator's admission decisions.
#[derive(Clone, Default)]
struct CancelRequests(Arc<AtomicU64>);

impl CancelRequests {
    /// The only constructor of Command::Cancel. Publish before enqueueing.
    fn request(&self, tx: &Sender<Command>) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        if tx.send(Command::Cancel).is_ok() {
            return true;
        }
        self.0.fetch_sub(1, Ordering::SeqCst);
        false
    }
}

/// Decide whether a key-up should be deferred (so auto-repeat can cancel it)
/// or a key-down cancels a deferred release. `hold_to_talk` is whether a
/// release currently ends the session: true for push-to-talk and for an
/// unlocked hold-or-toggle session, false for toggle and a locked session.
/// `held_binding` is the binding whose key we believe is down — the one
/// recording, or the one remembered while the pipeline is busy.
fn classify_ptt_event(
    pending_release_binding: Option<&str>,
    is_pressed: bool,
    hold_to_talk: bool,
    binding_id: &str,
    held_binding: Option<&str>,
) -> PttAction {
    if !hold_to_talk {
        return PttAction::Passthrough;
    }

    if is_pressed {
        if pending_release_binding == Some(binding_id) {
            PttAction::CancelRelease
        } else {
            PttAction::Passthrough
        }
    } else if held_binding == Some(binding_id) && pending_release_binding.is_none() {
        PttAction::DeferRelease
    } else {
        PttAction::Passthrough
    }
}

/// Pure lifecycle state machine: owns every transition decision (release
/// grace, hold-vs-tap classification, debounce, busy-pipeline
/// remember/forget, cancel, drain). Produces [`Effect`]s instead of touching
/// the app, so unit tests exercise the real production logic.
///
/// All three activation modes run through one machine. A recording starts on
/// key-down in every mode; what differs is how it ends:
///
/// * push-to-talk — every release stops (hold threshold of zero)
/// * toggle — releases are ignored, the next press stops (locked from the start)
/// * hold-or-toggle — a release after a long hold stops; a release after a
///   short tap locks the session, and the next press stops
struct PendingCapture {
    binding_id: String,
    hotkey_string: String,
    session: u64,
    pressed_at: Instant,
    locked: bool,
}
#[derive(Debug, PartialEq, Eq)]
enum CaptureResolution {
    Start(Effect),
    Failed,
    Discard,
}

struct CoordinatorState {
    stage: Stage,
    hold: Option<Hold>,
    last_press: Option<Instant>,
    pending_release: Option<PendingRelease>,
    pending_press: Option<PendingPress>,
    session: Option<DictationSession>,
    capture: Option<PendingCapture>,
    next_assistant_session: u64,
    /// Selection-command bindings whose busy press was rejected; each one's
    /// own release clears it, and its own repeats are ignored until then.
    assistant_rejected_held: BTreeSet<String>,
    cancels: CancelRequests,
    cancels_handled: u64,
}

impl CoordinatorState {
    fn new() -> Self {
        Self {
            stage: Stage::Idle,
            hold: None,
            last_press: None,
            pending_release: None,
            pending_press: None,
            session: None,
            capture: None,
            next_assistant_session: 1,
            assistant_rejected_held: BTreeSet::new(),
            cancels: CancelRequests::default(),
            cancels_handled: 0,
        }
    }

    fn cancel_queued(&self) -> bool {
        self.cancels.0.load(Ordering::SeqCst) > self.cancels_handled
    }

    /// Deadline of the deferred release, if any — drives `recv_timeout`.
    fn grace_deadline(&self) -> Option<Instant> {
        self.pending_release.as_ref().map(|p| p.deadline)
    }

    /// Whether the current session (recording, or remembered for the drain)
    /// outlives the key, so releases are ignored and the next press ends it.
    fn is_locked(&self) -> bool {
        self.hold.as_ref().is_some_and(|h| h.locked)
            || self.pending_press.as_ref().is_some_and(|p| p.locked)
            || self.capture.as_ref().is_some_and(|p| p.locked)
    }

    fn on_input(&mut self, input: InputEvent, now: Instant) -> Option<Effect> {
        let assistant =
            WorkflowMode::from_binding_id(&input.binding_id) == Some(WorkflowMode::Assistant);
        if assistant && self.assistant_rejected_held.contains(&input.binding_id) {
            if !input.is_pressed {
                self.assistant_rejected_held.remove(&input.binding_id);
            }
            return None;
        }
        let pending_release_binding = self
            .pending_release
            .as_ref()
            .map(|pending| pending.binding_id.as_str());
        let held_binding = match &self.stage {
            Stage::Capturing(id) | Stage::Recording(id) => Some(id.as_str()),
            Stage::Processing => self.pending_press.as_ref().map(|p| p.binding_id.as_str()),
            Stage::Idle => None,
        };
        let hold_to_talk = input.mode != ShortcutActivation::Toggle && !self.is_locked();

        match classify_ptt_event(
            pending_release_binding,
            input.is_pressed,
            hold_to_talk,
            &input.binding_id,
            held_binding,
        ) {
            PttAction::CancelRelease => {
                self.pending_release = None;
                return None;
            }
            PttAction::DeferRelease => {
                self.pending_release = Some(PendingRelease {
                    hold_threshold: input.effective_hold_threshold(),
                    binding_id: input.binding_id,
                    hotkey_string: input.hotkey_string,
                    deadline: now + RELEASE_GRACE,
                    released_at: now,
                });
                return None;
            }
            PttAction::Passthrough => {}
        }

        // Debounce rapid-fire press events (key repeat / double-tap).
        // Releases in the hold modes are deferred above to absorb X11 auto-repeat.
        // External triggers are exempt: each one is a deliberate edge from the
        // user's own integration, and dropping it desyncs toggle parity.
        if input.is_pressed && !input.external {
            if self
                .last_press
                .is_some_and(|t| now.duration_since(t) < DEBOUNCE)
            {
                debug!("Debounced press for '{}'", input.binding_id);
                return None;
            }
            self.last_press = Some(now);
        }

        if assistant
            && input.is_pressed
            && (matches!(self.stage, Stage::Processing)
                || matches!(&self.stage, Stage::Recording(id) if id != &input.binding_id))
        {
            self.assistant_rejected_held
                .insert(input.binding_id.clone());
            return Some(Effect::RejectAssistant);
        }
        if let Stage::Capturing(id) = &self.stage {
            if id != &input.binding_id {
                return None;
            }
            if input.is_pressed {
                if self.is_locked() || input.mode == ShortcutActivation::Toggle {
                    return self.cancel_capture();
                }
            } else if hold_to_talk {
                let threshold = input.effective_hold_threshold();
                let release = PendingRelease {
                    binding_id: input.binding_id,
                    hotkey_string: input.hotkey_string,
                    deadline: now,
                    released_at: now,
                    hold_threshold: threshold,
                };
                return self.finish_capture_hold(&release);
            }
            return None;
        }

        // A busy pipeline can't accept lifecycle changes now: classify the
        // input against any already-remembered press instead of dropping it
        // silently.
        if let Stage::Processing = self.stage {
            // Only one press can be remembered. Once a binding has claimed it,
            // inputs for a different binding are ignored — the same rule as a
            // different binding pressed while recording — rather than silently
            // replacing the remembered press and breaking its parity.
            if let Some(pending) = &self.pending_press {
                if pending.binding_id != input.binding_id {
                    debug!(
                        "Ignoring input for '{}': '{}' is already pending",
                        input.binding_id, pending.binding_id
                    );
                    return None;
                }
            }
            let remembered = self.pending_press.as_ref().map(|p| p.remembered());
            match classify_busy_input(input.is_pressed, input.mode, remembered) {
                BusyAction::Remember => {
                    debug!(
                        "Remembering press for '{}': pipeline busy",
                        input.binding_id
                    );
                    self.pending_press = Some(PendingPress {
                        // Toggle never ends on a release: locked from the start.
                        locked: input.mode == ShortcutActivation::Toggle,
                        binding_id: input.binding_id,
                        hotkey_string: input.hotkey_string,
                        pressed_at: now,
                    });
                }
                BusyAction::Forget => {
                    debug!("Forgetting remembered press for '{}'", input.binding_id);
                    self.pending_press = None;
                }
                BusyAction::Ignore => {
                    debug!("Ignoring input for '{}': pipeline busy", input.binding_id);
                }
            }
            return None;
        }

        if input.is_pressed {
            match &self.stage {
                Stage::Idle => {
                    if self.cancel_queued() {
                        debug!("Vetoing admission: cancellation outstanding");
                        return None;
                    }
                    if assistant {
                        let session = self.next_assistant_session;
                        self.next_assistant_session += 1;
                        self.stage = Stage::Capturing(input.binding_id.clone());
                        self.capture = Some(PendingCapture {
                            binding_id: input.binding_id.clone(),
                            hotkey_string: input.hotkey_string.clone(),
                            session,
                            pressed_at: now,
                            locked: input.mode == ShortcutActivation::Toggle,
                        });
                        return Some(Effect::BeginCapture {
                            binding_id: input.binding_id,
                            hotkey_string: input.hotkey_string,
                            session,
                        });
                    }
                    // Toggle never ends on a release: locked from the start.
                    let locked = input.mode == ShortcutActivation::Toggle;
                    return Some(self.begin_recording(
                        input.binding_id,
                        input.hotkey_string,
                        now,
                        locked,
                    ));
                }
                Stage::Recording(id) if id == &input.binding_id => {
                    // A locked session ends on the next press. In toggle mode
                    // every press ends it, even if the recording began under a
                    // hold mode (the setting changed mid-recording) — otherwise
                    // nothing but Escape could stop it.
                    if self.is_locked() || input.mode == ShortcutActivation::Toggle {
                        return Some(self.begin_processing(input.binding_id, input.hotkey_string));
                    }
                    // The key is still held (its release will end this
                    // recording), so a repeated press means nothing.
                    debug!("Ignoring press for '{}': key is held", input.binding_id);
                }
                _ => debug!(
                    "Ignoring press for '{}': another binding is recording",
                    input.binding_id
                ),
            }
        } else if hold_to_talk
            && matches!(&self.stage, Stage::Recording(id) if id == &input.binding_id)
        {
            // A release that was not deferred (one is already pending for this
            // binding): resolve it immediately rather than dropping it.
            let threshold = input.effective_hold_threshold();
            return self.finish_hold(input.binding_id, input.hotkey_string, now, threshold);
        }
        None
    }

    /// The `RELEASE_GRACE` window elapsed with no cancelling press arriving:
    /// resolve the deferred release against whatever that binding's key was
    /// holding — the live recording, or a press remembered while busy.
    fn on_grace_expired(&mut self) -> Option<Effect> {
        let pending = self.pending_release.take()?;
        match &self.stage {
            Stage::Capturing(id) if *id == pending.binding_id => self.finish_capture_hold(&pending),
            Stage::Recording(id) if *id == pending.binding_id => self.finish_hold(
                pending.binding_id,
                pending.hotkey_string,
                pending.released_at,
                pending.hold_threshold,
            ),
            Stage::Processing => {
                self.finish_pending_hold(&pending);
                None
            }
            _ => None,
        }
    }

    /// A press remembered while the pipeline was busy has been released for
    /// real, still before the drain. A completed hold has nothing left to
    /// start; a tap queues a locked session so the drain starts it — the
    /// same hold-vs-tap rule as [`CoordinatorState::finish_hold`].
    fn finish_pending_hold(&mut self, release: &PendingRelease) {
        let Some(pending) = self
            .pending_press
            .as_mut()
            .filter(|p| p.binding_id == release.binding_id)
        else {
            return;
        };
        let held = release
            .released_at
            .saturating_duration_since(pending.pressed_at);
        if held < release.hold_threshold {
            debug!(
                "Tap ({held:?}) for '{}' while busy: will start locked on when the pipeline drains",
                release.binding_id
            );
            pending.locked = true;
        } else {
            debug!(
                "Forgetting remembered press for '{}': released after a {held:?} hold while busy",
                release.binding_id
            );
            self.pending_press = None;
        }
    }

    /// The key that started the current recording has been released for real.
    /// A hold at least `threshold` long stops recording; anything shorter was a
    /// tap, which locks the session on until the next press.
    fn finish_hold(
        &mut self,
        binding_id: String,
        hotkey_string: String,
        released_at: Instant,
        threshold: Duration,
    ) -> Option<Effect> {
        let held = self
            .hold
            .as_ref()
            .map(|h| released_at.saturating_duration_since(h.pressed_at))
            // No hold bookkeeping means we cannot tell a tap from a hold;
            // stopping is the safe reading (it is what push-to-talk always did).
            .unwrap_or(Duration::MAX);
        if held >= threshold {
            return Some(self.begin_processing(binding_id, hotkey_string));
        }
        if let Some(hold) = &mut self.hold {
            debug!("Tap ({held:?}) for '{binding_id}': recording locked on until the next press");
            hold.locked = true;
        }
        None
    }

    fn cancel_capture(&mut self) -> Option<Effect> {
        let capture = self.capture.take()?;
        self.stage = Stage::Idle;
        self.pending_release = None;
        Some(Effect::CancelCapture {
            session: capture.session,
        })
    }
    fn finish_capture_hold(&mut self, release: &PendingRelease) -> Option<Effect> {
        let capture = self
            .capture
            .as_mut()
            .filter(|p| p.binding_id == release.binding_id)?;
        if release
            .released_at
            .saturating_duration_since(capture.pressed_at)
            >= release.hold_threshold
        {
            self.cancel_capture()
        } else {
            capture.locked = true;
            None
        }
    }
    fn on_capture_rejected(&mut self, session: u64) {
        if self.capture.as_ref().is_some_and(|p| p.session == session) {
            self.cancel_capture();
        }
    }
    fn on_capture_done(&mut self, session: u64, ok: bool) -> CaptureResolution {
        if !matches!(self.stage, Stage::Capturing(_))
            || !self.capture.as_ref().is_some_and(|p| p.session == session)
        {
            return CaptureResolution::Discard;
        }
        if !ok {
            self.cancel_capture();
            return CaptureResolution::Failed;
        }
        if self.cancel_queued() {
            self.cancel_capture();
            return CaptureResolution::Discard;
        }
        if let Some(release) = self.pending_release.take() {
            if self.finish_capture_hold(&release).is_some() {
                return CaptureResolution::Discard;
            }
        }
        let Some(capture) = self.capture.take() else {
            return CaptureResolution::Discard;
        };
        self.stage = Stage::Recording(capture.binding_id.clone());
        self.hold = Some(Hold {
            pressed_at: capture.pressed_at,
            locked: capture.locked,
        });
        CaptureResolution::Start(Effect::StartAssistant {
            binding_id: capture.binding_id,
            hotkey_string: capture.hotkey_string,
            session,
        })
    }
    fn on_cancel(&mut self, recording_was_active: bool) -> Option<Effect> {
        if self.cancel_queued() {
            self.cancels_handled += 1;
        }
        self.assistant_rejected_held.clear();
        let effect = self.cancel_capture();
        self.session = None;
        self.pending_release = None;
        // An explicit cancel abandons any remembered start too — the user
        // asked for silence, not a deferred recording.
        self.pending_press = None;
        // Don't reset during processing — wait for the pipeline to finish.
        if !matches!(self.stage, Stage::Processing)
            && (recording_was_active || matches!(self.stage, Stage::Recording(_)))
        {
            self.stage = Stage::Idle;
            self.hold = None;
        }
        effect
    }

    fn on_processing_finished(&mut self) -> Option<Effect> {
        self.session = None;
        self.stage = Stage::Idle;
        self.hold = None;
        if self.cancel_queued() {
            self.pending_press = None;
        }
        let pending = self.pending_press.take()?;
        debug!(
            "Pipeline drained; starting remembered press for '{}'",
            pending.binding_id
        );
        Some(self.begin_recording(
            pending.binding_id,
            pending.hotkey_string,
            pending.pressed_at,
            pending.locked,
        ))
    }

    /// Reconcile the optimistic `Stage::Recording` after the executor reports
    /// whether recording actually began (microphone access can be denied).
    fn on_start_result(&mut self, binding_id: &str, session: Option<DictationSession>) {
        if matches!(&self.stage, Stage::Recording(id) if id == binding_id) {
            if session.is_none() {
                self.stage = Stage::Idle;
                self.hold = None;
            }
            self.session = session;
        }
    }

    /// Optimistic transition to `Recording`; rolled back via
    /// [`CoordinatorState::on_start_result`] if the effect fails to start
    /// recording for real.
    fn begin_recording(
        &mut self,
        binding_id: String,
        hotkey_string: String,
        pressed_at: Instant,
        locked: bool,
    ) -> Effect {
        self.stage = Stage::Recording(binding_id.clone());
        self.hold = Some(Hold { pressed_at, locked });
        Effect::Start {
            binding_id,
            hotkey_string,
        }
    }

    fn begin_processing(&mut self, binding_id: String, hotkey_string: String) -> Effect {
        self.stage = Stage::Processing;
        self.hold = None;
        Effect::Stop {
            binding_id,
            hotkey_string,
        }
    }
}

/// Serialises all transcription lifecycle events through a single thread
/// to eliminate race conditions between keyboard shortcuts, signals, and
/// the async transcribe-paste pipeline. The thread is a thin shell: it
/// transports commands to the pure [`CoordinatorState`] and executes the
/// returned [`Effect`]s.
pub struct TranscriptionCoordinator {
    tx: Sender<Command>,
    cancels: CancelRequests,
}

impl TranscriptionCoordinator {
    pub fn new(app: AppHandle) -> Self {
        let (tx, rx) = mpsc::channel();

        let executor_tx = tx.clone();
        let mut state = CoordinatorState::new();
        let cancels = state.cancels.clone();
        thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut executor = Executor {
                    app,
                    tx: executor_tx,
                    intake: None,
                };

                loop {
                    let cmd = if let Some(deadline) = state.grace_deadline() {
                        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                            Ok(cmd) => cmd,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if let Some(effect) = state.on_grace_expired() {
                                    run_effect(&mut executor, &mut state, effect);
                                }
                                continue;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    } else {
                        match rx.recv() {
                            Ok(cmd) => cmd,
                            Err(_) => break,
                        }
                    };

                    handle_command(&mut executor, &mut state, cmd, &mut run_effect);
                }
                debug!("Transcription coordinator exited");
            }));
            if let Err(e) = result {
                error!("Transcription coordinator panicked: {e:?}");
            }
        });

        Self { tx, cancels }
    }

    /// Send a keyboard input event for a workflow binding (built-in or
    /// profile). `hold_threshold` only matters for
    /// [`ShortcutActivation::HoldOrToggle`].
    pub(crate) fn send_input(
        &self,
        binding_id: &str,
        hotkey_string: &str,
        is_pressed: bool,
        mode: ShortcutActivation,
        hold_threshold: Duration,
    ) {
        self.send(
            binding_id.to_string(),
            hotkey_string,
            is_pressed,
            mode,
            hold_threshold,
            false,
        );
    }

    /// Send an external trigger (SIGUSR2, CLI flag). Always a toggle press,
    /// always exempt from debounce — see [`InputEvent::external`].
    pub(crate) fn send_external_input(&self, workflow: WorkflowMode, source: &str) {
        self.send(
            workflow.binding_id().to_string(),
            source,
            true,
            ShortcutActivation::Toggle,
            Duration::ZERO,
            true,
        );
    }

    fn send(
        &self,
        binding_id: String,
        hotkey_string: &str,
        is_pressed: bool,
        mode: ShortcutActivation,
        hold_threshold: Duration,
        external: bool,
    ) {
        if self
            .tx
            .send(Command::Input(InputEvent {
                binding_id,
                hotkey_string: hotkey_string.to_string(),
                is_pressed,
                mode,
                hold_threshold,
                external,
            }))
            .is_err()
        {
            warn!("Transcription coordinator channel closed");
        }
    }

    pub(crate) fn request_cancel(&self) -> bool {
        let queued = self.cancels.request(&self.tx);
        if !queued {
            warn!("Transcription coordinator channel closed");
        }
        queued
    }

    pub fn notify_processing_finished(&self) {
        if self.tx.send(Command::ProcessingFinished).is_err() {
            warn!("Transcription coordinator channel closed");
        }
    }
}

struct Intake {
    session: u64,
    ticket: crate::selection::CaptureTicket,
    admission: crate::assistant::Admission,
    cancel_arm: crate::assistant::CancelArm,
    started: Instant,
}
struct Executor<C = AppHandle> {
    app: C,
    tx: Sender<Command>,
    intake: Option<Intake>,
}
/// Native action/audio boundary, substituted by deterministic runtime tests.
trait RecordingStartContext {
    fn start_action(
        &self,
        binding_id: &str,
        settings: &crate::settings::AppSettings,
        stt_language: Option<&str>,
    );
    fn is_recording(&self) -> bool;
    fn cancel_operation(&self) -> bool;
    /// Non-blocking cue confirming that a cancel stopped real work.
    fn cancel_feedback(&self);
    fn capture_error(&self, error: crate::assistant::AssistantError);
}
impl RecordingStartContext for AppHandle {
    fn start_action(
        &self,
        binding_id: &str,
        settings: &crate::settings::AppSettings,
        stt_language: Option<&str>,
    ) {
        TranscribeAction::start(self, binding_id, settings, stt_language);
    }
    fn is_recording(&self) -> bool {
        self.try_state::<Arc<AudioRecordingManager>>()
            .is_some_and(|audio| audio.is_recording())
    }
    fn cancel_operation(&self) -> bool {
        crate::utils::perform_cancellation(self)
    }
    fn cancel_feedback(&self) {
        crate::audio_feedback::play_feedback_sound(self, crate::audio_feedback::SoundType::Cancel);
    }

    fn capture_error(&self, error: crate::assistant::AssistantError) {
        error.emit(self);
    }
}
impl<C: RecordingStartContext> Executor<C> {
    fn accept_intake(&mut self, intake: Intake) {
        self.intake = Some(intake);
    }

    fn on_cancel(&mut self, state: &mut CoordinatorState) -> Option<Effect> {
        // Read before teardown: a cancel only confirms itself audibly when it
        // stopped a recording or an in-flight transcription/post-process.
        let busy = matches!(state.stage, Stage::Recording(_) | Stage::Processing);
        let was = self.app.cancel_operation();
        if was || busy {
            self.app.cancel_feedback();
        }
        state.on_cancel(was)
    }

    fn cancel_capture(&mut self, session: u64) {
        if self
            .intake
            .as_ref()
            .is_some_and(|intake| intake.session == session)
        {
            if let Some(intake) = self.intake.take() {
                intake.ticket.cancel();
            }
        }
    }

    fn capture_done(
        &mut self,
        state: &mut CoordinatorState,
        session: u64,
        result: Result<crate::selection::Captured, crate::selection::CaptureError>,
    ) {
        let resolution = state.on_capture_done(session, result.is_ok());
        let matching = self
            .intake
            .as_ref()
            .is_some_and(|intake| intake.session == session);
        match resolution {
            CaptureResolution::Start(Effect::StartAssistant {
                binding_id,
                hotkey_string: _,
                session: _,
            }) if matching => {
                if let (Some(intake), Ok(captured)) = (self.intake.take(), result) {
                    let assistant = crate::assistant::AssistantSession {
                        id: crate::selection::SessionId(session),
                        request: intake.admission.request,
                        captured: captured.text,
                        token: captured.token,
                        reservation: intake.admission.reservation,
                        clipboard_handling: intake.admission.clipboard_handling,
                        capture: intake.started.elapsed(),
                        cancel_arm: intake.cancel_arm,
                    };
                    let snapshot = DictationSession::assistant(
                        &intake.admission.settings,
                        &intake.admission.spec,
                        assistant,
                    );
                    let snapshot =
                        start_session(&self.app, &binding_id, &intake.admission.settings, snapshot);
                    state.on_start_result(&binding_id, snapshot);
                }
            }
            CaptureResolution::Failed => {
                if matching {
                    self.intake = None;
                }
                if let Err(error) = result {
                    if !matches!(
                        error,
                        crate::selection::CaptureError::Cancelled
                            | crate::selection::CaptureError::ShuttingDown
                    ) {
                        self.app
                            .capture_error(crate::assistant::AssistantError::new(
                                crate::assistant::AssistantStage::Capture,
                                error.code(),
                            ));
                    }
                }
            }
            _ => {
                if matching {
                    self.intake = None;
                }
            }
        }
    }
}
fn handle_command<C: RecordingStartContext>(
    executor: &mut Executor<C>,
    state: &mut CoordinatorState,
    cmd: Command,
    run: &mut impl FnMut(&mut Executor<C>, &mut CoordinatorState, Effect),
) {
    let effect = match cmd {
        Command::Input(input) => state.on_input(input, Instant::now()),
        Command::Cancel => executor.on_cancel(state),
        Command::CaptureDone { session, result } => {
            executor.capture_done(state, session, result);
            None
        }
        Command::ProcessingFinished => state.on_processing_finished(),
    };
    if let Some(effect) = effect {
        run(executor, state, effect);
    }
}

fn run_effect(executor: &mut Executor, state: &mut CoordinatorState, effect: Effect) {
    let app = &executor.app;
    match effect {
        Effect::BeginCapture {
            binding_id,
            hotkey_string: _,
            session,
        } => {
            let settings = get_settings(app);
            let store = app.state::<Arc<crate::assistant::recovery::RecoveryStore>>();
            match crate::assistant::admit_binding(
                crate::assistant::is_shutting_down(),
                &settings,
                &binding_id,
                &store,
            ) {
                Err(error) => {
                    state.on_capture_rejected(session);
                    error.error().emit(app);
                }
                Ok(admission) => {
                    let started = Instant::now();
                    let cancel_arm = crate::assistant::CancelArm::arm(app);
                    let tx = executor.tx.clone();
                    let ticket = crate::selection::begin_capture(
                        crate::selection::SessionId(session),
                        move |result| {
                            let _ = tx.send(Command::CaptureDone { session, result });
                        },
                    );
                    executor.accept_intake(Intake {
                        session,
                        ticket,
                        admission,
                        cancel_arm,
                        started,
                    });
                }
            }
        }
        Effect::CancelCapture { session } => executor.cancel_capture(session),
        Effect::RejectAssistant => {
            crate::assistant::AssistantError::new(crate::assistant::AssistantStage::Intake, "busy")
                .emit(app)
        }
        Effect::StartAssistant { .. } => unreachable!("capture completion owns assistant start"),
        Effect::Start {
            binding_id,
            hotkey_string,
        } => {
            let session = start(app, &binding_id, &hotkey_string);
            state.on_start_result(&binding_id, session);
        }
        Effect::Stop {
            binding_id,
            hotkey_string,
        } => {
            if let Some(session) = state.session.take() {
                TranscribeAction::stop(app, &binding_id, session);
            } else {
                warn!("No recording snapshot for '{binding_id}' ({hotkey_string})");
                if let Some(coordinator) = app.try_state::<TranscriptionCoordinator>() {
                    coordinator.notify_processing_finished();
                }
            }
        }
    }
}

/// Return the recording-start snapshot only when audio actually began. The
/// state machine rolls back its optimistic transition on failure.
fn start(app: &AppHandle, binding_id: &str, _hotkey_string: &str) -> Option<DictationSession> {
    start_with_settings(app, binding_id, &get_settings(app))
}

/// The profile is resolved here, inside the coordinator step that admitted
/// the press, so later edits never affect this session.
fn start_with_settings(
    app: &impl RecordingStartContext,
    binding_id: &str,
    settings: &crate::settings::AppSettings,
) -> Option<DictationSession> {
    let Some(session) = DictationSession::for_binding(binding_id, settings) else {
        warn!("No dictation workflow or profile for '{binding_id}'");
        return None;
    };
    start_session(app, binding_id, settings, session)
}

fn start_session(
    app: &impl RecordingStartContext,
    binding_id: &str,
    settings: &crate::settings::AppSettings,
    session: DictationSession,
) -> Option<DictationSession> {
    app.start_action(binding_id, settings, session.stt_language.as_deref());
    let recording = app.is_recording();
    if !recording {
        debug!("Start for '{binding_id}' did not begin recording; staying idle");
    }
    recording.then_some(session)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // CancelArm and shortcut ownership are process globals. Serialize only
    // fixtures that exercise those globals, including the utils order tests.
    pub(crate) static CANCEL_RUNTIME_TEST: tokio::sync::Mutex<()> =
        tokio::sync::Mutex::const_new(());

    #[derive(Default)]
    struct Runtime {
        mic: Option<u64>,
        stream: Option<u64>,
        stt: Option<u64>,
        tray: &'static str,
        overlay: bool,
        unloads: usize,
        starts: u64,
        stream_cancels: Vec<u64>,
        stt_kills: Vec<u64>,
        calls: Vec<(thread::ThreadId, &'static str)>,
        fallback: bool,
        stt_languages: Vec<Option<String>>,
        cancel_sounds: usize,
    }

    impl Runtime {
        fn call(&mut self, edge: &'static str) {
            self.calls.push((thread::current().id(), edge));
        }
    }

    #[derive(Clone)]
    struct FakeRuntime {
        runtime: Arc<std::sync::Mutex<Runtime>>,
        cancels: CancelRequests,
        tx: Sender<Command>,
        pause: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    }

    impl crate::shortcut::CancelShortcutContext for FakeRuntime {
        fn register_cancel_fallback(&self) {
            self.runtime.lock().unwrap().fallback = true;
        }
        fn unregister_cancel_fallback(&self) {
            let mut r = self.runtime.lock().unwrap();
            r.call("unregister");
            r.fallback = false;
        }
        fn schedule_cancel_reconcile(&self) {}
    }

    impl crate::utils::CancellationContext for FakeRuntime {
        fn cancel_audio(&self) -> bool {
            let mut r = self.runtime.lock().unwrap();
            r.call("audio");
            r.mic.take().is_some()
        }
        fn cancel_transcription(&self) {
            let mut r = self.runtime.lock().unwrap();
            r.call("transcription");
            if let Some(id) = r.stream.take() {
                r.stream_cancels.push(id);
            }
            if let Some(id) = r.stt.take() {
                r.stt_kills.push(id);
            }
        }
        fn reset_after_cancel(&self) {
            let mut r = self.runtime.lock().unwrap();
            r.call("reset");
            r.tray = "idle";
            r.overlay = false;
            r.unloads += 1;
        }
        fn request_cancel(&self) -> bool {
            let queued = self.cancels.request(&self.tx);
            if let Some((paused, resume)) = &self.pause {
                paused.wait();
                resume.wait();
            }
            queued
        }
    }

    impl RecordingStartContext for FakeRuntime {
        fn start_action(
            &self,
            _: &str,
            _: &crate::settings::AppSettings,
            stt_language: Option<&str>,
        ) {
            let mut r = self.runtime.lock().unwrap();
            r.stt_languages.push(stt_language.map(str::to_string));
            r.starts += 1;
            r.mic = Some(r.starts);
            r.stream = r.mic;
            r.tray = "recording";
            r.overlay = true;
        }
        fn is_recording(&self) -> bool {
            self.runtime.lock().unwrap().mic.is_some()
        }
        fn cancel_operation(&self) -> bool {
            crate::utils::perform_cancellation(self)
        }
        fn cancel_feedback(&self) {
            self.runtime.lock().unwrap().cancel_sounds += 1;
        }
        fn capture_error(&self, _: crate::assistant::AssistantError) {
            panic!("unexpected capture error")
        }
    }

    struct Harness {
        state: CoordinatorState,
        executor: Executor<FakeRuntime>,
        rx: mpsc::Receiver<Command>,
        store: Arc<crate::assistant::recovery::RecoveryStore>,
        effects: Vec<Effect>,
        /// What the coordinator step reads at admission.
        settings: crate::settings::AppSettings,
    }

    impl Harness {
        fn new() -> Self {
            let state = CoordinatorState::new();
            let (tx, rx) = mpsc::channel();
            let app = FakeRuntime {
                runtime: Arc::new(std::sync::Mutex::new(Runtime {
                    tray: "idle",
                    ..Runtime::default()
                })),
                cancels: state.cancels.clone(),
                tx: tx.clone(),
                pause: None,
            };
            Self {
                state,
                executor: Executor {
                    app,
                    tx,
                    intake: None,
                },
                rx,
                store: crate::assistant::recovery::RecoveryStore::new(8),
                effects: Vec::new(),
                settings: crate::assistant::tests::configured_settings("http://127.0.0.1:1"),
            }
        }
        fn drive(&mut self, cmd: Command) {
            let store = self.store.clone();
            let settings = self.settings.clone();
            let effects = &mut self.effects;
            handle_command(
                &mut self.executor,
                &mut self.state,
                cmd,
                &mut |ex, state, effect| match effect {
                    Effect::Start { binding_id, .. } => {
                        let snapshot = start_with_settings(&ex.app, &binding_id, &settings);
                        state.on_start_result(&binding_id, snapshot);
                    }
                    Effect::BeginCapture {
                        session,
                        binding_id,
                        ..
                    } => {
                        ex.accept_intake(Intake {
                            session,
                            ticket: crate::selection::CaptureTicket::test_pending(),
                            admission: crate::assistant::admit_binding(
                                false,
                                &settings,
                                &binding_id,
                                &store,
                            )
                            .ok()
                            .unwrap(),
                            cancel_arm: crate::assistant::CancelArm::arm(&ex.app),
                            started: Instant::now(),
                        });
                    }
                    Effect::CancelCapture { session } => ex.cancel_capture(session),
                    other => effects.push(other),
                },
            );
        }
        fn request(&self) {
            crate::utils::cancel_current_operation(&self.executor.app);
        }
        fn next(&mut self) {
            let cmd = self.rx.try_recv().expect("queued command");
            self.drive(cmd);
        }
        fn press(&mut self, mode: WorkflowMode) {
            self.drive(Command::Input(workflow_input(mode)));
        }
        fn captured(
            &self,
        ) -> (
            Command,
            Arc<std::sync::atomic::AtomicUsize>,
            Arc<std::sync::atomic::AtomicBool>,
        ) {
            use crate::selection::{Captured, CapturedText, SessionId, TargetId, TargetToken};
            use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize};
            let session = self.state.capture.as_ref().unwrap().session;
            let released = Arc::new(AtomicUsize::new(0));
            let early = Arc::new(AtomicBool::new(false));
            let r = released.clone();
            let e = early.clone();
            let runtime = self.executor.app.runtime.clone();
            (
                Command::CaptureDone {
                    session,
                    result: Ok(Captured {
                        text: CapturedText::Empty,
                        token: TargetToken::new(
                            TargetId(session),
                            SessionId(session),
                            true,
                            Arc::new(AtomicU8::new(0)),
                            Arc::new(move |_| {
                                e.store(runtime.lock().unwrap().mic.is_some(), Ordering::SeqCst);
                                r.fetch_add(1, Ordering::SeqCst);
                            }),
                        ),
                    }),
                },
                released,
                early,
            )
        }
        fn admit_assistant(
            &mut self,
        ) -> (
            Arc<std::sync::atomic::AtomicUsize>,
            Arc<std::sync::atomic::AtomicBool>,
        ) {
            self.press(WorkflowMode::Assistant);
            let (done, released, early) = self.captured();
            self.drive(done);
            assert_eq!(self.state.stage, Stage::Recording("assistant".into()));
            assert!(self.state.session.is_some());
            assert_eq!(released.load(Ordering::SeqCst), 0);
            (released, early)
        }
        fn assert_idle(&self) {
            assert_eq!(self.state.stage, Stage::Idle);
            assert!(self.executor.intake.is_none());
            assert!(self.state.session.is_none());
            assert!(self.executor.app.runtime.lock().unwrap().mic.is_none());
            assert_eq!(self.store.free_slots(), 8);
            assert!(!crate::assistant::cancel_armed());
            assert!(!self.executor.app.runtime.lock().unwrap().fallback);
        }
    }

    #[test]
    fn assistant_executor_uncancelled_admission_retains_recording_session() {
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        let mut h = Harness::new();
        let (released, early) = h.admit_assistant();
        assert_eq!(h.executor.app.runtime.lock().unwrap().starts, 1);
        assert_eq!(h.store.free_slots(), 7);
        h.request();
        h.next();
        h.assert_idle();
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert!(!early.load(Ordering::SeqCst));
    }

    fn delayed_caller(raw_successor: bool) {
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        let mut h = Harness::new();
        let (released, early) = h.admit_assistant();
        let paused = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        let mut caller = h.executor.app.clone();
        caller.pause = Some((paused.clone(), resume.clone()));
        let t = thread::spawn(move || crate::utils::cancel_current_operation(&caller));
        let caller_id = t.thread().id();
        paused.wait();
        struct ParkedCaller {
            resume: Arc<std::sync::Barrier>,
            thread: Option<thread::JoinHandle<()>>,
        }
        impl ParkedCaller {
            fn release(&mut self) {
                if let Some(t) = self.thread.take() {
                    self.resume.wait();
                    t.join().unwrap();
                }
            }
        }
        impl Drop for ParkedCaller {
            fn drop(&mut self) {
                self.release();
            }
        }
        let mut parked = ParkedCaller {
            resume,
            thread: Some(t),
        };
        // Save assertions until the parked caller can always be resumed and joined.
        let before = h.executor.app.runtime.lock().unwrap().calls.clone();
        h.next();
        let teardown = {
            let r = h.executor.app.runtime.lock().unwrap();
            (r.stream_cancels.clone(), r.tray, r.overlay, r.unloads)
        };
        let was_idle =
            h.state.stage == Stage::Idle && h.state.session.is_none() && h.store.free_slots() == 8;
        let successor_release = if raw_successor {
            h.press(WorkflowMode::Raw);
            None
        } else {
            Some(h.admit_assistant().0)
        };
        {
            let mut r = h.executor.app.runtime.lock().unwrap();
            r.stt = r.mic;
        }
        parked.release();
        assert!(
            before.is_empty(),
            "caller must enqueue without destructive edges"
        );
        assert_eq!(teardown, (vec![1], "idle", false, 1));
        assert!(was_idle);
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert!(
            !early.load(Ordering::SeqCst),
            "stop microphone before releasing A's target"
        );
        assert!(matches!(h.rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let r = h.executor.app.runtime.lock().unwrap();
        assert_eq!(r.mic, Some(2));
        assert_eq!(r.stream, Some(2));
        assert_eq!(r.stt, Some(2));
        assert_eq!(r.stream_cancels, [1]);
        assert!(r.stt_kills.is_empty());
        assert_eq!(r.tray, "recording");
        assert!(r.overlay);
        assert_eq!(r.unloads, 1);
        assert!(r.calls.iter().all(|(id, _)| *id != caller_id));
        drop(r);
        assert_eq!(
            h.state.stage,
            Stage::Recording(
                if raw_successor {
                    "transcribe"
                } else {
                    "assistant"
                }
                .into()
            )
        );
        assert!(h.state.session.is_some());
        if let Some(released) = successor_release {
            assert_eq!(released.load(Ordering::SeqCst), 0);
        }
        h.request();
        h.next();
        h.assert_idle();
    }

    #[test]
    fn delayed_cancel_caller_cannot_touch_successor_stream_or_stt() {
        delayed_caller(false);
    }
    #[test]
    fn delayed_cancel_caller_cannot_touch_raw_successor() {
        delayed_caller(true);
    }

    #[test]
    fn cancel_before_capture_done_never_opens_microphone() {
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        let mut h = Harness::new();
        h.press(WorkflowMode::Assistant);
        let (done, released, early) = h.captured();
        h.request();
        h.executor.tx.send(done).unwrap();
        h.next();
        h.next();
        h.assert_idle();
        assert_eq!(h.executor.app.runtime.lock().unwrap().starts, 0);
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert!(!early.load(Ordering::SeqCst));
    }

    #[test]
    fn cancel_requested_while_capture_done_queued_vetoes_start() {
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        let mut h = Harness::new();
        h.press(WorkflowMode::Assistant);
        let (done, released, early) = h.captured();
        h.executor.tx.send(done).unwrap();
        h.request();
        h.next();
        h.assert_idle();
        assert_eq!(h.executor.app.runtime.lock().unwrap().starts, 0);
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert!(!early.load(Ordering::SeqCst));
        h.next();
        h.assert_idle();
    }

    #[test]
    fn cancel_after_capture_start_stops_audio_before_target_release() {
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        let mut h = Harness::new();
        let (released, early) = h.admit_assistant();
        h.request();
        assert!(h.executor.app.is_recording());
        h.next();
        h.assert_idle();
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert!(!early.load(Ordering::SeqCst));
        let r = h.executor.app.runtime.lock().unwrap();
        let edges: Vec<_> = r.calls.iter().map(|(_, edge)| *edge).collect();
        assert_eq!(edges, ["audio", "transcription", "reset", "unregister"]);
    }

    #[test]
    fn cancel_requested_before_drain_abandons_remembered_press() {
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        let mut h = Harness::new();
        h.state.stage = Stage::Processing;
        h.press(WorkflowMode::Raw);
        assert!(h.state.pending_press.is_some());
        h.executor.tx.send(Command::ProcessingFinished).unwrap();
        h.request();
        h.next();
        assert_eq!(h.executor.app.runtime.lock().unwrap().starts, 0);
        assert!(h.state.pending_press.is_none());
        assert_eq!(h.state.stage, Stage::Idle);
        h.next();
        h.assert_idle();
    }

    #[test]
    fn cancel_handled_after_remembered_start_stops_it() {
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        let mut h = Harness::new();
        h.state.stage = Stage::Processing;
        h.press(WorkflowMode::Raw);
        h.drive(Command::ProcessingFinished);
        assert!(h.executor.app.is_recording());
        assert!(h.state.session.is_some());
        h.request();
        h.next();
        h.assert_idle();
        assert_eq!(h.executor.app.runtime.lock().unwrap().starts, 1);
    }

    #[test]
    fn cancel_requested_after_press_queued_is_never_ignored() {
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        for workflow in [WorkflowMode::Assistant, WorkflowMode::Raw] {
            for handled_first in [false, true] {
                let mut h = Harness::new();
                h.executor
                    .tx
                    .send(Command::Input(workflow_input(workflow)))
                    .unwrap();
                if handled_first {
                    h.next();
                }
                h.request();
                if !handled_first {
                    h.next();
                }
                h.next();
                h.assert_idle();
                assert_eq!(
                    h.executor.app.runtime.lock().unwrap().starts,
                    u64::from(handled_first && workflow == WorkflowMode::Raw)
                );
            }
        }
    }

    #[test]
    fn cancel_burst_vetoes_admission_until_last_cancel_handled() {
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        let mut h = Harness::new();
        h.request();
        h.request();
        h.next();
        h.press(WorkflowMode::Raw);
        h.assert_idle();
        assert_eq!(h.executor.app.runtime.lock().unwrap().starts, 0);
        h.next();
        h.press(WorkflowMode::Raw);
        assert_eq!(h.state.stage, Stage::Recording("transcribe".into()));
        assert_eq!(h.executor.app.runtime.lock().unwrap().starts, 1);
        h.request();
        h.next();
        h.assert_idle();
    }

    #[test]
    fn cancel_during_processing_kills_only_current_stt() {
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        let mut h = Harness::new();
        h.press(WorkflowMode::Raw);
        h.state.stage = Stage::Processing;
        {
            let mut r = h.executor.app.runtime.lock().unwrap();
            r.mic = None;
            r.stream = None;
            r.stt = Some(1);
        }
        h.request();
        h.next();
        assert_eq!(h.state.stage, Stage::Processing);
        h.press(WorkflowMode::Raw);
        h.drive(Command::ProcessingFinished);
        {
            let mut r = h.executor.app.runtime.lock().unwrap();
            r.stt = Some(2);
            assert_eq!(r.stt_kills, [1]);
            assert_eq!(r.mic, Some(2));
        }
        assert!(matches!(h.rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert_eq!(h.executor.app.runtime.lock().unwrap().stt, Some(2));
        h.request();
        h.next();
        h.assert_idle();
    }

    #[test]
    fn cancel_sound_only_when_cancel_stopped_work() {
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        let sounds = |h: &Harness| h.executor.app.runtime.lock().unwrap().cancel_sounds;

        // No-op cancel while idle: silent.
        let mut h = Harness::new();
        h.request();
        h.next();
        h.assert_idle();
        assert_eq!(sounds(&h), 0);

        // Cancel during recording: one cue.
        let mut h = Harness::new();
        h.press(WorkflowMode::Raw);
        assert!(h.executor.app.is_recording());
        h.request();
        h.next();
        h.assert_idle();
        assert_eq!(sounds(&h), 1);
        // A second cancel after teardown is a no-op again.
        h.request();
        h.next();
        assert_eq!(sounds(&h), 1);

        // Cancel during in-flight transcription (microphone already closed).
        let mut h = Harness::new();
        h.press(WorkflowMode::Raw);
        h.state.stage = Stage::Processing;
        {
            let mut r = h.executor.app.runtime.lock().unwrap();
            r.mic = None;
            r.stream = None;
            r.stt = Some(1);
        }
        h.request();
        h.next();
        assert_eq!(sounds(&h), 1);
    }

    #[test]
    fn cancel_queued_vetoes_idle_press_drain_start_and_capture_start() {
        let (tx, rx) = mpsc::channel();
        let mut state = CoordinatorState::new();
        assert!(state.cancels.request(&tx));
        assert!(state
            .on_input(workflow_input(WorkflowMode::Raw), Instant::now())
            .is_none());
        assert!(state
            .on_input(workflow_input(WorkflowMode::Assistant), Instant::now())
            .is_none());
        assert_eq!(state.stage, Stage::Idle);
        state.on_cancel(false);
        assert!(matches!(rx.try_recv(), Ok(Command::Cancel)));
        state.stage = Stage::Processing;
        state.on_input(workflow_input(WorkflowMode::Raw), Instant::now());
        assert!(state.cancels.request(&tx));
        assert!(state.on_processing_finished().is_none());
        assert!(state.pending_press.is_none());
        state.on_cancel(false);
        assert!(matches!(rx.try_recv(), Ok(Command::Cancel)));
        let session = capture(&mut state, ShortcutActivation::Toggle, Instant::now());
        assert!(state.cancels.request(&tx));
        assert_eq!(
            state.on_capture_done(session, true),
            CaptureResolution::Discard
        );
        assert_eq!(state.stage, Stage::Idle);
        assert!(state.capture.is_none());
        state.on_cancel(false);
        state.on_cancel(false); // Unrequested direct calls cannot drift.
        assert!(matches!(rx.try_recv(), Ok(Command::Cancel)));
        assert!(state.cancels.request(&tx));
        assert!(state.cancel_queued());
    }

    #[test]
    fn closed_cancel_channel_rolls_back_request_for_inline_fallback() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let state = CoordinatorState::new();
        assert!(!state.cancels.request(&tx));
        assert!(!state.cancel_queued());
    }

    #[derive(Clone)]
    struct CancelTestContext {
        cancels: CancelRequests,
        tx: Sender<Command>,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        fallback: std::sync::Arc<std::sync::atomic::AtomicBool>,
        registrations: std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
    }

    impl crate::shortcut::CancelShortcutContext for CancelTestContext {
        fn register_cancel_fallback(&self) {
            self.fallback
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        fn unregister_cancel_fallback(&self) {
            self.fallback
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        fn schedule_cancel_reconcile(&self) {
            self.registrations
                .lock()
                .unwrap()
                .push(crate::shortcut::cancel_requested());
        }
    }
    impl crate::utils::CancellationContext for CancelTestContext {
        fn cancel_audio(&self) -> bool {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            false
        }
        fn cancel_transcription(&self) {}
        fn reset_after_cancel(&self) {}
        fn request_cancel(&self) -> bool {
            self.cancels.request(&self.tx)
        }
    }

    impl RecordingStartContext for CancelTestContext {
        fn start_action(&self, _: &str, _: &crate::settings::AppSettings, _: Option<&str>) {
            panic!("unexpected start")
        }
        fn is_recording(&self) -> bool {
            false
        }
        fn cancel_operation(&self) -> bool {
            crate::utils::perform_cancellation(self)
        }
        fn cancel_feedback(&self) {}
        fn capture_error(&self, _: crate::assistant::AssistantError) {
            panic!("unexpected capture error")
        }
    }

    #[tokio::test]
    async fn explicit_cancel_during_blocked_commit_keeps_escape_until_ack_then_raw_owns_it() {
        use crate::assistant::{
            self, recovery::RecoveryStore, tests as fixtures, CancelArm, CommitTracker,
            SessionOutcome,
        };
        use crate::llm_client::test_http::FakeServer;
        use crate::selection::{CapturedText, ClipboardSettlement, CommitOutcome, RefuseReason};
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        };

        static TRACKER: CommitTracker = CommitTracker::new();
        let server = FakeServer::new(vec![fixtures::successful_server_step("generated")]).await;
        let store = RecoveryStore::new(8);
        let (mut session, released) = fixtures::session(&server.url, CapturedText::Empty, &store);
        let _guard = CANCEL_RUNTIME_TEST.lock().await;
        let mut state = CoordinatorState::new();
        let (tx, rx) = mpsc::channel();
        let context = CancelTestContext {
            cancels: state.cancels.clone(),
            tx: tx.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
            fallback: Arc::new(AtomicBool::new(false)),
            registrations: Arc::new(Mutex::new(Vec::new())),
        };
        state.stage = Stage::Processing;
        session.cancel_arm = CancelArm::arm(&context);
        let cancelled = Arc::clone(&context.cancelled);
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (acknowledge, wait) = std::sync::mpsc::channel();
        let task = tokio::spawn(assistant::complete_session(
            session,
            "instruction",
            Arc::new(move || cancelled.load(Ordering::SeqCst)),
            move |request| {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!((request.is_cancelled)());
                CommitOutcome::Refused {
                    reason: RefuseReason::Cancelled,
                    clipboard: ClipboardSettlement::Restored,
                }
            },
            &TRACKER,
        ));
        ready.await.unwrap();

        // Escape, CLI --cancel and tray all call this production entry point.
        crate::utils::cancel_current_operation(&context);
        assert!(
            !context.cancelled.load(Ordering::SeqCst),
            "request performs no caller-side teardown"
        );
        let mut executor = Executor {
            app: context.clone(),
            tx,
            intake: None,
        };
        handle_command(
            &mut executor,
            &mut state,
            rx.try_recv().unwrap(),
            &mut |_, _, _| panic!("unexpected effect"),
        );
        assert!(context.cancelled.load(Ordering::SeqCst));
        assert!(assistant::cancel_armed());
        assert!(
            crate::shortcut::cancel_requested(),
            "Escape must stay owned while strict abort/settlement is blocked"
        );
        assert!(context.fallback.load(Ordering::SeqCst));
        assert_eq!(*context.registrations.lock().unwrap(), [true]);
        assert_eq!(state.stage, Stage::Processing);
        assert_eq!(released.load(Ordering::SeqCst), 0);
        assert_eq!(store.free_slots(), 7);

        // A fresh raw press after cancellation is remembered until acknowledgment.
        assert!(state
            .on_input(input(ShortcutActivation::Toggle, true), Instant::now())
            .is_none());
        acknowledge.send(()).unwrap();
        assert!(matches!(
            task.await.unwrap().outcome,
            SessionOutcome::Discarded
        ));
        assert!(!assistant::cancel_armed());
        assert!(!crate::shortcut::cancel_requested());
        assert!(!context.fallback.load(Ordering::SeqCst));
        assert_eq!(*context.registrations.lock().unwrap(), [true, false]);
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert_eq!(store.free_slots(), 8);
        assert!(store.items().is_empty());
        assert!(
            matches!(state.on_processing_finished(), Some(Effect::Start { binding_id, .. }) if binding_id == "transcribe")
        );
        crate::shortcut::register_cancel_shortcut(&context);
        assert!(crate::shortcut::cancel_requested());
        assert!(context.fallback.load(Ordering::SeqCst));
        assert_eq!(*context.registrations.lock().unwrap(), [true, false, true]);
        crate::shortcut::unregister_cancel_shortcut(&context);
        assert_eq!(
            *context.registrations.lock().unwrap(),
            [true, false, true, false]
        );
    }

    fn assistant_input(mode: ShortcutActivation, pressed: bool) -> InputEvent {
        InputEvent {
            binding_id: "assistant".into(),
            hotkey_string: "test".into(),
            is_pressed: pressed,
            mode,
            hold_threshold: Duration::from_millis(300),
            external: false,
        }
    }
    fn capture(state: &mut CoordinatorState, mode: ShortcutActivation, now: Instant) -> u64 {
        let Some(Effect::BeginCapture { session, .. }) =
            state.on_input(assistant_input(mode, true), now)
        else {
            panic!("capture expected")
        };
        session
    }
    #[test]
    fn assistant_capture_success_starts_locked_toggle_recording() {
        let mut state = CoordinatorState::new();
        let id = capture(&mut state, ShortcutActivation::Toggle, Instant::now());
        assert!(
            matches!(state.on_capture_done(id, true), CaptureResolution::Start(Effect::StartAssistant { session, .. }) if session == id)
        );
        assert_eq!(state.stage, Stage::Recording("assistant".into()));
        assert!(state.is_locked());
    }
    #[test]
    fn assistant_capture_failure_returns_idle_without_start() {
        let mut state = CoordinatorState::new();
        let id = capture(&mut state, ShortcutActivation::Toggle, Instant::now());
        assert_eq!(state.on_capture_done(id, false), CaptureResolution::Failed);
        assert_eq!(state.stage, Stage::Idle);
        assert!(state.capture.is_none());
    }
    #[test]
    fn assistant_capture_rejected_by_admission_returns_idle() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        let id = capture(&mut state, ShortcutActivation::Toggle, now);
        state.on_capture_rejected(id);
        assert_eq!(state.stage, Stage::Idle);
        assert!(matches!(
            state.on_input(ptt_input(true), now + Duration::from_secs(1)),
            Some(Effect::Start { .. })
        ));
    }
    #[test]
    fn stale_capture_result_is_discarded() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        let first = capture(&mut state, ShortcutActivation::Toggle, now);
        assert_eq!(
            state.on_cancel(false),
            Some(Effect::CancelCapture { session: first })
        );
        let second = capture(
            &mut state,
            ShortcutActivation::Toggle,
            now + Duration::from_secs(1),
        );
        assert_ne!(first, second);
        assert_eq!(
            state.on_capture_done(first, true),
            CaptureResolution::Discard
        );
        assert_eq!(state.stage, Stage::Capturing("assistant".into()));
        assert!(matches!(
            state.on_capture_done(second, true),
            CaptureResolution::Start(_)
        ));
    }
    #[test]
    fn ptt_release_during_capture_cancels_pending_session() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        let id = capture(&mut state, ShortcutActivation::PushToTalk, now);
        assert!(state
            .on_input(
                assistant_input(ShortcutActivation::PushToTalk, false),
                now + Duration::from_millis(40)
            )
            .is_none());
        assert_eq!(
            state.on_grace_expired(),
            Some(Effect::CancelCapture { session: id })
        );
        assert_eq!(state.on_capture_done(id, true), CaptureResolution::Discard);
        assert_eq!(state.stage, Stage::Idle);
    }
    #[test]
    fn ptt_release_in_grace_when_capture_completes_discards() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        let id = capture(&mut state, ShortcutActivation::PushToTalk, now);
        state.on_input(
            assistant_input(ShortcutActivation::PushToTalk, false),
            now + Duration::from_millis(20),
        );
        assert_eq!(state.on_capture_done(id, true), CaptureResolution::Discard);
        assert_eq!(state.stage, Stage::Idle);
        assert!(state.grace_deadline().is_none());
    }
    #[test]
    fn hold_or_toggle_tap_during_capture_starts_locked() {
        for grace_elapsed in [false, true] {
            let mut state = CoordinatorState::new();
            let now = Instant::now();
            let id = capture(&mut state, ShortcutActivation::HoldOrToggle, now);
            state.on_input(
                assistant_input(ShortcutActivation::HoldOrToggle, false),
                now + Duration::from_millis(100),
            );
            if grace_elapsed {
                assert!(state.on_grace_expired().is_none());
            }
            assert!(matches!(
                state.on_capture_done(id, true),
                CaptureResolution::Start(_)
            ));
            assert!(state.is_locked());
            assert!(matches!(
                state.on_input(
                    assistant_input(ShortcutActivation::HoldOrToggle, true),
                    now + Duration::from_secs(1)
                ),
                Some(Effect::Stop { .. })
            ));
        }
    }
    #[test]
    fn hold_or_toggle_long_hold_released_during_capture_cancels() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        let id = capture(&mut state, ShortcutActivation::HoldOrToggle, now);
        state.on_input(
            assistant_input(ShortcutActivation::HoldOrToggle, false),
            now + Duration::from_millis(300),
        );
        assert_eq!(
            state.on_grace_expired(),
            Some(Effect::CancelCapture { session: id })
        );
        assert_eq!(state.on_capture_done(id, true), CaptureResolution::Discard);
    }
    #[test]
    fn toggle_second_press_during_capture_cancels() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        let id = capture(&mut state, ShortcutActivation::Toggle, now);
        assert_eq!(
            state.on_input(
                assistant_input(ShortcutActivation::Toggle, true),
                now + Duration::from_millis(100)
            ),
            Some(Effect::CancelCapture { session: id })
        );
        assert_eq!(state.stage, Stage::Idle);
    }
    #[test]
    fn cancel_during_capture_emits_cancel_capture() {
        let mut state = CoordinatorState::new();
        let id = capture(&mut state, ShortcutActivation::PushToTalk, Instant::now());
        assert_eq!(
            state.on_cancel(false),
            Some(Effect::CancelCapture { session: id })
        );
        assert_eq!(state.on_cancel(false), None);
        assert_eq!(state.stage, Stage::Idle);
    }
    #[test]
    fn assistant_press_while_busy_is_rejected_not_remembered() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        drive_into_processing(&mut state, now);
        state.on_input(toggle_input(true), now + Duration::from_millis(200));
        assert_eq!(
            state.on_input(
                assistant_input(ShortcutActivation::Toggle, true),
                now + Duration::from_millis(300)
            ),
            Some(Effect::RejectAssistant)
        );
        assert_eq!(state.pending_press.as_ref().unwrap().binding_id, BINDING);
        assert!(
            matches!(state.on_processing_finished(), Some(Effect::Start { binding_id, .. }) if binding_id == BINDING)
        );
    }
    #[test]
    fn assistant_press_while_raw_recording_is_rejected() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        state.on_input(ptt_input(true), now);
        assert_eq!(
            state.on_input(
                assistant_input(ShortcutActivation::Toggle, true),
                now + Duration::from_millis(100)
            ),
            Some(Effect::RejectAssistant)
        );
        assert_eq!(state.stage, Stage::Recording(BINDING.into()));
        assert!(state.pending_press.is_none());
    }
    #[test]
    fn rejected_assistant_release_and_repeat_are_consumed_after_drain() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        drive_into_processing(&mut state, now);
        assert_eq!(
            state.on_input(
                assistant_input(ShortcutActivation::PushToTalk, true),
                now + Duration::from_millis(100)
            ),
            Some(Effect::RejectAssistant)
        );
        assert!(state.on_processing_finished().is_none());
        assert!(state
            .on_input(
                assistant_input(ShortcutActivation::PushToTalk, true),
                now + Duration::from_millis(200)
            )
            .is_none());
        assert!(state
            .on_input(
                assistant_input(ShortcutActivation::PushToTalk, false),
                now + Duration::from_millis(300)
            )
            .is_none());
        assert!(matches!(
            state.on_input(
                assistant_input(ShortcutActivation::PushToTalk, true),
                now + Duration::from_millis(400)
            ),
            Some(Effect::BeginCapture { .. })
        ));
    }
    #[test]
    fn raw_press_during_assistant_processing_is_remembered_until_drain() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        let id = capture(&mut state, ShortcutActivation::Toggle, now);
        state.on_capture_done(id, true);
        assert!(matches!(
            state.on_input(
                assistant_input(ShortcutActivation::Toggle, true),
                now + Duration::from_secs(1)
            ),
            Some(Effect::Stop { .. })
        ));
        assert!(state
            .on_input(ptt_input(true), now + Duration::from_secs(2))
            .is_none());
        assert_eq!(state.stage, Stage::Processing);
        assert!(
            matches!(state.on_processing_finished(), Some(Effect::Start { binding_id, .. }) if binding_id == BINDING)
        );
    }
    #[test]
    fn full_recovery_rejects_assistant_but_not_raw() {
        let store = crate::assistant::recovery::RecoveryStore::new(8);
        let slots: Vec<_> = (0..8).map(|_| store.reserve().unwrap()).collect();
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        let id = capture(&mut state, ShortcutActivation::Toggle, now);
        assert!(matches!(
            crate::assistant::admit_binding(
                false,
                &crate::assistant::tests::configured_settings("http://unused.invalid"),
                "assistant",
                &store
            ),
            Err(crate::assistant::IntakeRejection::RecoveryFull)
        ));
        state.on_capture_rejected(id);
        assert!(matches!(
            state.on_input(ptt_input(true), now + Duration::from_secs(1)),
            Some(Effect::Start { .. })
        ));
        drop(slots);
    }
    #[test]
    fn raw_and_cleanup_inputs_during_capture_are_ignored() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        let id = capture(&mut state, ShortcutActivation::Toggle, now);
        for (n, workflow) in [WorkflowMode::Raw, WorkflowMode::Cleanup]
            .into_iter()
            .enumerate()
        {
            assert!(state
                .on_input(
                    workflow_input(workflow),
                    now + Duration::from_millis(100 + n as u64 * 100)
                )
                .is_none());
        }
        assert_eq!(state.capture.as_ref().unwrap().session, id);
        assert!(state.pending_press.is_none());
    }

    async fn coordinator_assistant_case(
        text: crate::selection::CapturedText,
        step: Step,
        outcome: crate::selection::CommitOutcome,
        expected_calls: u32,
    ) {
        static TRACKER: crate::assistant::CommitTracker = crate::assistant::CommitTracker::new();
        let server = FakeServer::new(vec![step]).await;
        let store = crate::assistant::recovery::RecoveryStore::new(8);
        let settings = crate::assistant::tests::configured_settings(&server.url);
        let spec = crate::settings::spec_for_binding(&settings, "assistant").unwrap();
        let (session, released) = crate::assistant::tests::session(&server.url, text, &store);
        let mut state = CoordinatorState::new();
        state.next_assistant_session = session.id.0;
        let now = Instant::now();
        let id = capture(&mut state, ShortcutActivation::Toggle, now);
        let (tx, _) = mpsc::channel();
        let mut executor = Executor {
            app: FakeRuntime {
                runtime: Arc::new(std::sync::Mutex::new(Runtime::default())),
                cancels: state.cancels.clone(),
                tx: tx.clone(),
                pause: None,
            },
            tx,
            intake: None,
        };
        executor.accept_intake(Intake {
            session: id,
            ticket: crate::selection::CaptureTicket::test_pending(),
            admission: crate::assistant::Admission {
                request: session.request,
                reservation: session.reservation,
                clipboard_handling: session.clipboard_handling,
                settings,
                spec,
            },
            cancel_arm: session.cancel_arm,
            started: now,
        });
        executor.capture_done(
            &mut state,
            id,
            Ok(crate::selection::Captured {
                text: session.captured,
                token: session.token,
            }),
        );
        assert!(
            executor.app.is_recording(),
            "Executor must start instruction audio before the pipeline"
        );
        assert_eq!(executor.app.runtime.lock().unwrap().starts, 1);
        assert_eq!(released.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(matches!(
            state.on_input(
                assistant_input(ShortcutActivation::Toggle, true),
                now + Duration::from_secs(1)
            ),
            Some(Effect::Stop { .. })
        ));
        // Simulate the audio/STT boundary after the real coordinator stop edge.
        executor.app.runtime.lock().unwrap().mic = None;
        let session = state
            .session
            .take()
            .unwrap()
            .into_assistant()
            .unwrap_or_else(|_| panic!());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let report = crate::assistant::complete_session(
            session,
            "spoken instruction",
            crate::assistant::tests::never_cancel(),
            move |req| {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(req.replacement, "generated");
                assert_ne!(req.replacement, "spoken instruction");
                outcome
            },
            &TRACKER,
        )
        .await;
        assert_eq!(state.stage, Stage::Processing);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            expected_calls as usize
        );
        assert_eq!(report.commit_calls, expected_calls);
        assert_eq!(released.load(std::sync::atomic::Ordering::SeqCst), 1);
        if expected_calls == 0 {
            assert!(matches!(
                report.outcome,
                crate::assistant::SessionOutcome::Failed(_)
            ));
            assert_eq!(store.free_slots(), 8);
        }
        if matches!(
            report.outcome,
            crate::assistant::SessionOutcome::Recoverable(_)
        ) {
            assert_eq!(store.items()[0].generated, "generated");
        }
        assert!(state.on_processing_finished().is_none());
        assert_eq!(state.stage, Stage::Idle);
    }
    fn assistant_delivered() -> crate::selection::CommitOutcome {
        crate::selection::CommitOutcome::Delivered {
            clipboard: crate::selection::ClipboardSettlement::Restored,
        }
    }
    #[tokio::test]
    async fn coordinator_selected_replacement_end_to_end() {
        coordinator_assistant_case(
            crate::selection::CapturedText::Selected("source".into()),
            crate::assistant::tests::successful_server_step("generated"),
            assistant_delivered(),
            1,
        )
        .await;
    }
    #[tokio::test]
    async fn coordinator_empty_insertion_end_to_end() {
        coordinator_assistant_case(
            crate::selection::CapturedText::Empty,
            crate::assistant::tests::successful_server_step("generated"),
            assistant_delivered(),
            1,
        )
        .await;
    }
    #[tokio::test]
    async fn coordinator_whitespace_selection_end_to_end() {
        coordinator_assistant_case(
            crate::selection::CapturedText::Selected(" \t\n".into()),
            crate::assistant::tests::successful_server_step("generated"),
            assistant_delivered(),
            1,
        )
        .await;
    }
    #[tokio::test]
    async fn coordinator_llm_failure_before_mutation() {
        coordinator_assistant_case(
            crate::selection::CapturedText::Selected("original".into()),
            Step::Respond(503, "offline".into()),
            assistant_delivered(),
            0,
        )
        .await;
    }
    #[tokio::test]
    async fn coordinator_stale_target_is_recoverable_not_pasted() {
        for reason in [
            crate::selection::StaleReason::FocusChanged,
            crate::selection::StaleReason::SelectionChanged,
        ] {
            coordinator_assistant_case(
                crate::selection::CapturedText::Selected("original".into()),
                crate::assistant::tests::successful_server_step("generated"),
                crate::selection::CommitOutcome::Refused {
                    reason: crate::selection::RefuseReason::TargetStale(reason),
                    clipboard: crate::selection::ClipboardSettlement::Untouched,
                },
                1,
            )
            .await;
        }
    }
    #[tokio::test]
    async fn coordinator_assistant_commits_at_most_once() {
        coordinator_assistant_case(
            crate::selection::CapturedText::Empty,
            crate::assistant::tests::successful_server_step("generated"),
            crate::selection::CommitOutcome::Uncertain {
                reason: crate::selection::UncertainReason::NoReceipt,
                clipboard: crate::selection::ClipboardSettlement::Restored,
            },
            1,
        )
        .await;
    }
    #[tokio::test]
    async fn coordinator_drain_waits_for_commit_settlement() {
        static TRACKER: crate::assistant::CommitTracker = crate::assistant::CommitTracker::new();
        let server = FakeServer::new(vec![crate::assistant::tests::successful_server_step(
            "generated",
        )])
        .await;
        let store = crate::assistant::recovery::RecoveryStore::new(8);
        let (session, _) = crate::assistant::tests::session(
            &server.url,
            crate::selection::CapturedText::Empty,
            &store,
        );
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        let id = capture(&mut state, ShortcutActivation::Toggle, now);
        state.on_capture_done(id, true);
        state.on_input(
            assistant_input(ShortcutActivation::Toggle, true),
            now + Duration::from_secs(1),
        );
        state.on_input(ptt_input(true), now + Duration::from_secs(2));
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let mut task = tokio::spawn(crate::assistant::complete_session(
            session,
            "instruction",
            crate::assistant::tests::never_cancel(),
            move |_| {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(3)).unwrap();
                assistant_delivered()
            },
            &TRACKER,
        ));
        ready.await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut task)
            .await
            .is_err());
        assert_eq!(state.stage, Stage::Processing);
        assert_eq!(state.pending_press.as_ref().unwrap().binding_id, BINDING);
        release.send(()).unwrap();
        assert!(matches!(
            task.await.unwrap().outcome,
            crate::assistant::SessionOutcome::Delivered
        ));
        assert!(matches!(
            state.on_processing_finished(),
            Some(Effect::Start { .. })
        ));
    }
    #[tokio::test]
    async fn coordinator_cancel_during_llm_never_commits() {
        static TRACKER: crate::assistant::CommitTracker = crate::assistant::CommitTracker::new();
        let server = FakeServer::new(vec![Step::StallBeforeHeaders]).await;
        let store = crate::assistant::recovery::RecoveryStore::new(8);
        let (session, _) = crate::assistant::tests::session(
            &server.url,
            crate::selection::CapturedText::Empty,
            &store,
        );
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        let id = capture(&mut state, ShortcutActivation::Toggle, now);
        state.on_capture_done(id, true);
        state.on_input(
            assistant_input(ShortcutActivation::Toggle, true),
            now + Duration::from_secs(1),
        );
        state.on_input(ptt_input(true), now + Duration::from_secs(2));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = Arc::clone(&cancelled);
        let task = tokio::spawn(crate::assistant::complete_session(
            session,
            "instruction",
            Arc::new(move || probe.load(std::sync::atomic::Ordering::SeqCst)),
            |_| panic!("cancel forbids mutation"),
            &TRACKER,
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while server.requests.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(state.on_cancel(false).is_none());
        cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .outcome,
            crate::assistant::SessionOutcome::Discarded
        ));
        assert!(state.on_processing_finished().is_none());
        assert_eq!(store.free_slots(), 8);
    }

    #[test]
    fn assistant_press_begins_capture_before_any_start() {
        let mut state = CoordinatorState::new();
        let effect = state.on_input(
            InputEvent {
                binding_id: "assistant".into(),
                hotkey_string: "test".into(),
                is_pressed: true,
                mode: ShortcutActivation::Toggle,
                hold_threshold: Duration::ZERO,
                external: false,
            },
            Instant::now(),
        );
        assert!(
            matches!(effect, Some(Effect::BeginCapture { .. })),
            "assistant admission must begin capture, got {effect:?}"
        );
        assert!(matches!(state.stage, Stage::Capturing(_)));
    }
    use crate::actions::process_transcription_output;
    use crate::llm_client::test_http::{completion, FakeServer, Step};

    fn workflow_input(workflow: WorkflowMode) -> InputEvent {
        InputEvent {
            binding_id: workflow.binding_id().into(),
            hotkey_string: "test".into(),
            is_pressed: true,
            mode: ShortcutActivation::Toggle,
            hold_threshold: Duration::ZERO,
            external: true,
        }
    }

    #[tokio::test]
    async fn coordinator_keeps_cleanup_snapshot_until_stop_and_delivers_fallback() {
        let selected = FakeServer::new(vec![Step::Respond(503, "offline".into())]).await;
        let other = FakeServer::new(vec![]).await;
        let mut settings = crate::settings::get_default_settings();
        settings.post_process_provider_id = "custom".into();
        settings.post_process_providers =
            vec![crate::llm_client::test_http::provider(&selected.url)];
        settings
            .post_process_models
            .insert("custom".into(), "start-model".into());
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        assert!(matches!(
            state.on_input(workflow_input(WorkflowMode::Cleanup), now),
            Some(Effect::Start { .. })
        ));
        state.on_start_result(
            WorkflowMode::Cleanup.binding_id(),
            Some(DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap()),
        );
        settings.post_process_providers[0].base_url = other.url.clone();
        settings
            .post_process_models
            .insert("custom".into(), "edited-model".into());
        settings.post_process_selected_prompt_id = None;
        assert!(matches!(
            state.on_input(
                workflow_input(WorkflowMode::Cleanup),
                now + Duration::from_secs(1)
            ),
            Some(Effect::Stop { .. })
        ));
        let session = state
            .session
            .take()
            .expect("stop owns the recording-start snapshot");
        assert_eq!(session.mode(), WorkflowMode::Cleanup);
        let output = process_transcription_output(&session, "recoverable raw").await;
        assert_eq!(output.final_text, "recoverable raw");
        assert!(output
            .cleanup_error
            .as_deref()
            .unwrap()
            .starts_with("llm_provider"));
        assert_eq!(selected.requests.lock().unwrap()[0]["model"], "start-model");
        assert!(other.requests.lock().unwrap().is_empty());
        assert!(state.on_processing_finished().is_none());
        assert_eq!(state.stage, Stage::Idle);
        assert!(matches!(
            state.on_input(
                workflow_input(WorkflowMode::Raw),
                now + Duration::from_secs(2)
            ),
            Some(Effect::Start { .. })
        ));
    }

    #[tokio::test]
    async fn coordinator_raw_snapshot_never_sends_to_configured_provider() {
        let server = FakeServer::new(vec![]).await;
        let mut settings = crate::settings::get_default_settings();
        settings.post_process_enabled = true;
        settings.post_process_provider_id = "custom".into();
        settings.post_process_providers = vec![crate::llm_client::test_http::provider(&server.url)];
        settings
            .post_process_models
            .insert("custom".into(), "model".into());
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        state.on_input(workflow_input(WorkflowMode::Raw), now);
        state.on_start_result(
            WorkflowMode::Raw.binding_id(),
            Some(DictationSession::snapshot(WorkflowMode::Raw, &settings).unwrap()),
        );
        // Another workflow cannot alter the recording already in progress.
        assert!(state
            .on_input(
                workflow_input(WorkflowMode::Cleanup),
                now + Duration::from_secs(1)
            )
            .is_none());
        assert!(matches!(
            state.on_input(
                workflow_input(WorkflowMode::Raw),
                now + Duration::from_secs(2)
            ),
            Some(Effect::Stop { .. })
        ));
        let session = state.session.take().unwrap();
        let output = process_transcription_output(&session, "TFDQuery.").await;
        assert_eq!(output.final_text, "TFDQuery.");
        assert_eq!(session.mode(), WorkflowMode::Raw);
        assert!(server.requests.lock().unwrap().is_empty());
        assert!(output.post_processed_text.is_none());
    }

    #[tokio::test]
    async fn remembered_cleanup_starts_with_new_snapshot_after_raw_drains() {
        let server = FakeServer::new(vec![Step::Respond(
            200,
            completion(Some(r#"{"transcription":"Cleaned."}"#), "stop"),
        )])
        .await;
        let mut settings = crate::settings::get_default_settings();
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        state.on_input(workflow_input(WorkflowMode::Raw), now);
        state.on_start_result(
            WorkflowMode::Raw.binding_id(),
            Some(DictationSession::snapshot(WorkflowMode::Raw, &settings).unwrap()),
        );
        state.on_input(
            workflow_input(WorkflowMode::Raw),
            now + Duration::from_secs(1),
        );
        state.session.take();
        assert!(state
            .on_input(
                workflow_input(WorkflowMode::Cleanup),
                now + Duration::from_secs(2)
            )
            .is_none());
        settings.post_process_provider_id = "custom".into();
        settings.post_process_providers = vec![crate::llm_client::test_http::provider(&server.url)];
        settings
            .post_process_models
            .insert("custom".into(), "next-model".into());
        let Some(Effect::Start { binding_id, .. }) = state.on_processing_finished() else {
            panic!("remembered cleanup must start when raw drains")
        };
        assert_eq!(binding_id, WorkflowMode::Cleanup.binding_id());
        state.on_start_result(
            &binding_id,
            Some(DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap()),
        );
        state.on_input(
            workflow_input(WorkflowMode::Cleanup),
            now + Duration::from_secs(3),
        );
        let output = process_transcription_output(&state.session.take().unwrap(), "raw").await;
        assert_eq!(output.final_text, "Cleaned.");
        assert_eq!(server.requests.lock().unwrap()[0]["model"], "next-model");
    }

    #[test]
    fn cancel_and_failed_start_release_mode_snapshot_and_allow_new_session() {
        let mut state = CoordinatorState::new();
        let settings = crate::settings::get_default_settings();
        let now = Instant::now();
        for mode in [WorkflowMode::Raw, WorkflowMode::Cleanup] {
            state.on_input(workflow_input(mode), now);
            state.on_start_result(
                mode.binding_id(),
                Some(DictationSession::snapshot(mode, &settings).unwrap()),
            );
            assert!(state.session.is_some());
            state.on_cancel(true);
            assert!(state.session.is_none());
            assert_eq!(state.stage, Stage::Idle);
            assert!(matches!(
                state.on_input(workflow_input(mode), now),
                Some(Effect::Start { .. })
            ));
            state.on_start_result(mode.binding_id(), None);
            assert!(state.session.is_none());
            assert_eq!(state.stage, Stage::Idle);
        }
    }

    // -----------------------------------------------------------------------
    // HANDY-008: profile bindings
    // -----------------------------------------------------------------------

    const DICTATION_A: &str = "dictation_profile_pa";
    const DICTATION_B: &str = "dictation_profile_pb";
    const COMMAND_X: &str = "command_profile_px";
    const COMMAND_Y: &str = "command_profile_py";

    fn input_for(binding_id: &str, mode: ShortcutActivation, is_pressed: bool) -> InputEvent {
        InputEvent {
            binding_id: binding_id.into(),
            hotkey_string: binding_id.into(),
            is_pressed,
            mode,
            hold_threshold: Duration::from_millis(300),
            external: false,
        }
    }

    fn profile(
        id: &str,
        kind: crate::settings::ProfileKind,
        speech_language: Option<&str>,
    ) -> crate::settings::ProcessingProfile {
        crate::settings::ProcessingProfile {
            id: id.into(),
            name: format!("Profile {id}"),
            kind,
            prompt_id: Some(crate::settings::BUILTIN_ASSISTANT_ID.into()),
            model: None,
            speech_language: speech_language.map(Into::into),
            english_cleanup: None,
        }
    }

    #[test]
    fn handy008_second_dictation_profile_is_ignored_while_first_records() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        assert!(matches!(
            state.on_input(toggle_input_for(DICTATION_A, false), now),
            Some(Effect::Start { binding_id, .. }) if binding_id == DICTATION_A
        ));
        assert!(state
            .on_input(
                toggle_input_for(DICTATION_B, false),
                now + Duration::from_millis(100)
            )
            .is_none());
        assert_eq!(state.stage, Stage::Recording(DICTATION_A.into()));
        assert!(matches!(
            state.on_input(
                toggle_input_for(DICTATION_A, false),
                now + Duration::from_millis(200)
            ),
            Some(Effect::Stop { binding_id, .. }) if binding_id == DICTATION_A
        ));
    }

    #[test]
    fn handy008_dictation_profile_press_during_processing_starts_at_drain() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        drive_into_processing(&mut state, now);
        assert!(state
            .on_input(
                toggle_input_for(DICTATION_A, false),
                now + Duration::from_millis(200)
            )
            .is_none());
        assert_eq!(
            state.pending_press.as_ref().unwrap().binding_id,
            DICTATION_A
        );
        assert!(matches!(
            state.on_processing_finished(),
            Some(Effect::Start { binding_id, .. }) if binding_id == DICTATION_A
        ));
        assert_eq!(state.stage, Stage::Recording(DICTATION_A.into()));
    }

    #[test]
    fn handy008_command_profile_rejections_are_held_per_binding() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        let ms = |n: u64| now + Duration::from_millis(n);
        // A command press while raw records is rejected, never remembered.
        assert!(matches!(
            state.on_input(ptt_input(true), now),
            Some(Effect::Start { .. })
        ));
        assert_eq!(
            state.on_input(
                input_for(COMMAND_X, ShortcutActivation::PushToTalk, true),
                ms(100)
            ),
            Some(Effect::RejectAssistant)
        );
        assert!(state.pending_press.is_none());
        assert_eq!(
            state.on_input(
                input_for("assistant", ShortcutActivation::PushToTalk, true),
                ms(200)
            ),
            Some(Effect::RejectAssistant)
        );
        assert_eq!(
            state.assistant_rejected_held,
            BTreeSet::from([COMMAND_X.to_string(), "assistant".to_string()])
        );
        // Its own repeat is ignored while held.
        assert!(state
            .on_input(
                input_for(COMMAND_X, ShortcutActivation::PushToTalk, true),
                ms(300)
            )
            .is_none());
        // The rejected binding's release clears only its own entry.
        assert!(state
            .on_input(
                input_for(COMMAND_X, ShortcutActivation::PushToTalk, false),
                ms(400)
            )
            .is_none());
        assert_eq!(
            state.assistant_rejected_held,
            BTreeSet::from(["assistant".to_string()])
        );
        // Stop raw (a held push-to-talk release resolves after the grace).
        state.on_input(ptt_input(false), ms(500));
        assert!(matches!(
            state.on_grace_expired(),
            Some(Effect::Stop { .. })
        ));
        assert!(state.on_processing_finished().is_none());
        // A different command binding pressed after Processing ends captures.
        assert!(matches!(
            state.on_input(input_for(COMMAND_Y, ShortcutActivation::Toggle, true), ms(700)),
            Some(Effect::BeginCapture { binding_id, .. }) if binding_id == COMMAND_Y
        ));
        // The still-held assistant binding stays consumed until its release.
        assert_eq!(
            state.assistant_rejected_held,
            BTreeSet::from(["assistant".to_string()])
        );
        assert!(matches!(
            state.on_cancel(false),
            Some(Effect::CancelCapture { .. })
        ));
        assert!(
            state.assistant_rejected_held.is_empty(),
            "cancel clears all"
        );
    }

    #[test]
    fn handy008_cancel_queued_vetoes_profile_press_and_command_capture_done() {
        let (tx, _rx) = mpsc::channel();
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        assert!(state.cancels.request(&tx));
        assert!(state
            .on_input(toggle_input_for(DICTATION_A, false), now)
            .is_none());
        assert!(state
            .on_input(
                input_for(COMMAND_X, ShortcutActivation::Toggle, true),
                now + Duration::from_millis(100)
            )
            .is_none());
        assert_eq!(state.stage, Stage::Idle);
        state.on_cancel(false);
        let Some(Effect::BeginCapture { session, .. }) = state.on_input(
            input_for(COMMAND_X, ShortcutActivation::Toggle, true),
            now + Duration::from_millis(200),
        ) else {
            panic!("capture expected")
        };
        assert!(state.cancels.request(&tx));
        assert_eq!(
            state.on_capture_done(session, true),
            CaptureResolution::Discard
        );
        assert_eq!(state.stage, Stage::Idle);
    }

    #[test]
    fn handy008_executor_passes_profile_speech_language_to_recording_start() {
        use crate::settings::ProfileKind;
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        let mut h = Harness::new();
        h.settings
            .processing_profiles
            .iter_mut()
            .find(|p| p.id == crate::settings::BUILTIN_CLEANUP_PROFILE_ID)
            .unwrap()
            .speech_language = Some("pl".into());
        h.settings
            .processing_profiles
            .push(profile("pa", ProfileKind::Dictation, Some("de")));
        for binding in [
            WorkflowMode::Cleanup.binding_id(),
            DICTATION_A,
            WorkflowMode::Raw.binding_id(),
        ] {
            h.drive(Command::Input(toggle_input_for(binding, true)));
            assert_eq!(h.state.stage, Stage::Recording(binding.into()));
            let session = h.state.session.as_ref().unwrap();
            assert_eq!(
                session.profile.as_ref().map(|p| p.id.as_str()),
                match binding {
                    "transcribe" => None,
                    "transcribe_with_post_process" => {
                        Some(crate::settings::BUILTIN_CLEANUP_PROFILE_ID)
                    }
                    _ => Some("pa"),
                }
            );
            h.request();
            h.next();
            h.assert_idle();
        }
        assert_eq!(
            h.executor.app.runtime.lock().unwrap().stt_languages,
            [Some("pl".to_string()), Some("de".to_string()), None]
        );
        // A deleted custom profile never starts recording.
        h.drive(Command::Input(toggle_input_for(DICTATION_B, true)));
        h.assert_idle();
        assert_eq!(h.executor.app.runtime.lock().unwrap().starts, 3);
    }

    #[test]
    fn handy008_command_profile_is_admitted_and_snapshotted_at_press() {
        use crate::settings::ProfileKind;
        let _guard = CANCEL_RUNTIME_TEST.blocking_lock();
        let mut h = Harness::new();
        h.settings.processing_profiles.push(profile(
            "px",
            ProfileKind::SelectionCommand,
            Some("pl"),
        ));
        h.drive(Command::Input(toggle_input_for(COMMAND_X, true)));
        assert_eq!(h.state.stage, Stage::Capturing(COMMAND_X.into()));
        // Edits after admission do not reach this session.
        h.settings.processing_profiles.retain(|p| p.id != "px");
        let (done, released, _) = h.captured();
        h.drive(done);
        assert_eq!(h.state.stage, Stage::Recording(COMMAND_X.into()));
        let session = h.state.session.as_ref().unwrap();
        assert_eq!(session.profile.as_ref().unwrap().id, "px");
        assert_eq!(session.stt_language.as_deref(), Some("pl"));
        assert_eq!(
            h.executor.app.runtime.lock().unwrap().stt_languages,
            [Some("pl".to_string())]
        );
        h.request();
        h.next();
        h.assert_idle();
        assert_eq!(released.load(Ordering::SeqCst), 1);
        // A missing profile is a configuration rejection, before capacity.
        let store = crate::assistant::recovery::RecoveryStore::new(1);
        let _slot = store.reserve().unwrap();
        let Err(rejection) = crate::assistant::admit_binding(false, &h.settings, COMMAND_X, &store)
        else {
            panic!("missing profile must be rejected")
        };
        let error = rejection.error();
        assert_eq!(error.code, "llm_config");
        assert_eq!(error.reason, Some("profile_not_found"));
    }

    #[test]
    fn push_to_talk_release_while_recording_defers_release() {
        assert_eq!(
            classify_ptt_event(None, false, true, "transcribe", Some("transcribe")),
            PttAction::DeferRelease
        );
    }

    #[test]
    fn push_to_talk_press_matching_pending_release_cancels_release() {
        assert_eq!(
            classify_ptt_event(
                Some("transcribe"),
                true,
                true,
                "transcribe",
                Some("transcribe")
            ),
            PttAction::CancelRelease
        );
    }

    #[test]
    fn toggle_mode_press_and_release_pass_through() {
        assert_eq!(
            classify_ptt_event(
                Some("transcribe"),
                true,
                false,
                "transcribe",
                Some("transcribe")
            ),
            PttAction::Passthrough
        );
        assert_eq!(
            classify_ptt_event(None, false, false, "transcribe", Some("transcribe")),
            PttAction::Passthrough
        );
    }

    #[test]
    fn press_for_different_binding_than_pending_release_passes_through() {
        assert_eq!(
            classify_ptt_event(
                Some("transcribe"),
                true,
                true,
                "transcribe_with_post_process",
                Some("transcribe")
            ),
            PttAction::Passthrough
        );
    }

    #[test]
    fn press_matching_pending_release_cancels_without_recording_state() {
        assert_eq!(
            classify_ptt_event(Some("transcribe"), true, true, "transcribe", None),
            PttAction::CancelRelease
        );
    }

    // ---------------------------------------------------------------------
    // Busy-pipeline input classification.
    //
    // Toggle-style triggers (SIGUSR2, CLI flags, pedals that signal on both
    // edges) flip state on every edge. Dropping a press that arrives while
    // the previous pipeline is still processing desyncs the parity: the next
    // edge then starts a recording no one will stop, leaving the overlay
    // waiting for input with the button long released.
    // ---------------------------------------------------------------------

    #[test]
    fn toggle_press_during_processing_remembers_start() {
        assert_eq!(
            classify_busy_input(true, ShortcutActivation::Toggle, None),
            BusyAction::Remember
        );
    }

    #[test]
    fn second_toggle_press_during_processing_forgets_press() {
        assert_eq!(
            classify_busy_input(true, ShortcutActivation::Toggle, Some(Remembered::Locked)),
            BusyAction::Forget
        );
    }

    #[test]
    fn toggle_release_during_processing_is_ignored() {
        assert_eq!(
            classify_busy_input(false, ShortcutActivation::Toggle, None),
            BusyAction::Ignore
        );
        assert_eq!(
            classify_busy_input(false, ShortcutActivation::Toggle, Some(Remembered::Locked)),
            BusyAction::Ignore
        );
    }

    #[test]
    fn hold_modes_classify_busy_inputs_by_pending_state() {
        let cases = [
            (true, None, BusyAction::Remember),
            (true, Some(Remembered::Held), BusyAction::Ignore),
            (true, Some(Remembered::Locked), BusyAction::Forget),
            (false, None, BusyAction::Ignore),
            (false, Some(Remembered::Held), BusyAction::Ignore),
            (false, Some(Remembered::Locked), BusyAction::Ignore),
        ];

        for mode in [
            ShortcutActivation::PushToTalk,
            ShortcutActivation::HoldOrToggle,
        ] {
            for (is_pressed, remembered, expected) in cases {
                assert_eq!(classify_busy_input(is_pressed, mode, remembered), expected);
            }
        }
    }

    /// Toggle parity across a busy window: an odd number of presses remembers
    /// one start, each further press flips the remembered press off/on again.
    #[test]
    fn toggle_presses_alternate_remember_and_forget_while_busy() {
        let mut remembered = None;
        for expected in [
            BusyAction::Remember,
            BusyAction::Forget,
            BusyAction::Remember,
        ] {
            let action = classify_busy_input(true, ShortcutActivation::Toggle, remembered);
            assert_eq!(action, expected);
            remembered = (action == BusyAction::Remember).then_some(Remembered::Locked);
        }
        assert!(remembered.is_some());
    }

    // ---------------------------------------------------------------------
    // Sequence-level regression coverage for issue #1539.
    //
    // Under X11 key auto-repeat, holding a push-to-talk key does not emit one
    // long press. It emits the initial press followed by a stream of
    // synthesized release/press pairs, then a single genuine release on key-up.
    // Before the fix, every synthesized release passed straight through and
    // stopped recording, so holding the key "rapidly toggled" recording on and
    // off. The fix defers each release for a short grace window and cancels it
    // when the matching auto-repeat press arrives.
    //
    // The unit tests above assert the classifiers in isolation. The harness
    // below drives the real `CoordinatorState` through whole event sequences
    // — the same `on_input` / `on_grace_expired` handlers the coordinator
    // thread runs — so a burst can be exercised deterministically without a
    // Tauri AppHandle or real timers, and the tests can never drift from the
    // production transitions.
    // ---------------------------------------------------------------------

    const BINDING: &str = "transcribe";

    #[derive(Clone, Copy)]
    enum Ev {
        /// A key-down event (real initial press or a synthesized auto-repeat press).
        Press,
        /// A key-up event (synthesized auto-repeat release or the genuine key-up).
        Release,
        /// The `RELEASE_GRACE` window elapsed with no cancelling press arriving.
        Grace,
    }

    struct DriveResult {
        starts: u32,
        stops: u32,
        stage: Stage,
    }

    fn ptt_input(is_pressed: bool) -> InputEvent {
        InputEvent {
            binding_id: BINDING.to_string(),
            hotkey_string: BINDING.to_string(),
            is_pressed,
            mode: ShortcutActivation::PushToTalk,
            hold_threshold: Duration::ZERO,
            external: false,
        }
    }

    /// Feeds an event sequence to a real [`CoordinatorState`] the way the
    /// coordinator thread would; effects are counted instead of executed.
    fn drive(events: &[Ev]) -> DriveResult {
        let mut state = CoordinatorState::new();
        let mut clock = Instant::now();
        let mut starts = 0u32;
        let mut stops = 0u32;

        for ev in events {
            // Auto-repeat events arrive a few ms apart, well inside DEBOUNCE.
            clock += Duration::from_millis(5);

            let effect = match ev {
                Ev::Grace => state.on_grace_expired(),
                Ev::Press | Ev::Release => {
                    state.on_input(ptt_input(matches!(ev, Ev::Press)), clock)
                }
            };
            match effect {
                Some(Effect::Start { .. }) => starts += 1,
                Some(Effect::Stop { .. }) => stops += 1,
                None => {}
                other => panic!("unexpected raw effect: {other:?}"),
            }
        }

        DriveResult {
            starts,
            stops,
            stage: state.stage,
        }
    }

    /// Initial press plus several synthesized release/press pairs, as X11 emits
    /// while a push-to-talk key is held down.
    fn autorepeat_burst() -> Vec<Ev> {
        let mut events = vec![Ev::Press];
        for _ in 0..6 {
            events.push(Ev::Release);
            events.push(Ev::Press);
        }
        events
    }

    /// Regression for #1539: a burst of X11 auto-repeat release/press pairs must
    /// not stop recording. Before the fix the first synthesized release stopped
    /// recording immediately (stops == 1, stage left Recording), which produced
    /// the rapid on/off toggling. With the fix the releases are coalesced and
    /// recording stays continuously active for the whole burst.
    #[test]
    fn x11_autorepeat_burst_does_not_toggle_recording() {
        let result = drive(&autorepeat_burst());
        assert_eq!(result.starts, 1, "recording should start exactly once");
        assert_eq!(
            result.stops, 0,
            "synthesized auto-repeat releases must not stop recording mid-burst"
        );
        assert_eq!(
            result.stage,
            Stage::Recording(BINDING.to_string()),
            "recording must remain active across the entire auto-repeat burst"
        );
    }

    /// Complements the burst test: once the key is genuinely released and the
    /// grace window elapses with no re-press, recording stops exactly once. This
    /// proves the debounce only coalesces synthesized releases and does not wedge
    /// the coordinator or swallow the real key-up.
    #[test]
    fn genuine_release_after_grace_stops_recording_once() {
        let mut events = autorepeat_burst();
        events.push(Ev::Release); // genuine key-up
        events.push(Ev::Grace); // grace window elapses, no cancelling press
        let result = drive(&events);
        assert_eq!(result.starts, 1, "recording should start exactly once");
        assert_eq!(
            result.stops, 1,
            "a genuine release should stop recording exactly once"
        );
        assert_eq!(result.stage, Stage::Processing);
    }

    // ---------------------------------------------------------------------
    // Sequence-level coverage of the busy-pipeline and cancel paths, driven
    // through the real machine.
    // ---------------------------------------------------------------------

    /// PTT press while the pipeline is busy is remembered and starts recording
    /// once the pipeline drains.
    #[test]
    fn press_during_processing_starts_after_drain() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();

        let effect = state.on_input(ptt_input(true), now);
        assert!(matches!(effect, Some(Effect::Start { .. })));

        let effect = state.on_input(ptt_input(false), now + Duration::from_millis(100));
        assert!(effect.is_none(), "release should be deferred, not fired");

        let effect = state.on_grace_expired();
        assert!(matches!(effect, Some(Effect::Stop { .. })));

        let effect = state.on_input(ptt_input(true), now + Duration::from_millis(200));
        assert!(effect.is_none(), "busy pipeline must remember, not start");

        let effect = state.on_processing_finished();
        assert!(
            matches!(effect, Some(Effect::Start { .. })),
            "remembered press should start once the pipeline drains"
        );
    }

    /// Two toggle presses inside one busy window net to no-op: nothing starts
    /// when the pipeline drains (toggle parity).
    #[test]
    fn toggle_presses_during_processing_net_noop_after_drain() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();

        let effect = state.on_input(ptt_input(true), now);
        assert!(matches!(effect, Some(Effect::Start { .. })));
        let effect = state.on_input(ptt_input(false), now + Duration::from_millis(100));
        assert!(effect.is_none());
        let effect = state.on_grace_expired();
        assert!(matches!(effect, Some(Effect::Stop { .. })));

        let toggle = |state: &mut CoordinatorState, at: Instant| {
            state.on_input(
                InputEvent {
                    binding_id: BINDING.to_string(),
                    hotkey_string: BINDING.to_string(),
                    is_pressed: true,
                    mode: ShortcutActivation::Toggle,
                    hold_threshold: Duration::ZERO,
                    external: true,
                },
                at,
            )
        };

        let effect = toggle(&mut state, now + Duration::from_millis(200));
        assert!(effect.is_none());
        let effect = toggle(&mut state, now + Duration::from_millis(300));
        assert!(effect.is_none());

        let effect = state.on_processing_finished();
        assert!(
            effect.is_none(),
            "even number of busy toggle presses must not start recording"
        );
        assert_eq!(state.stage, Stage::Idle);
    }

    /// Cancel while processing abandons a remembered press: the pipeline drains
    /// to idle and nothing starts.
    #[test]
    fn cancel_during_processing_drops_remembered_press() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();

        let effect = state.on_input(ptt_input(true), now);
        assert!(matches!(effect, Some(Effect::Start { .. })));
        let effect = state.on_input(ptt_input(false), now + Duration::from_millis(100));
        assert!(effect.is_none());
        let effect = state.on_grace_expired();
        assert!(matches!(effect, Some(Effect::Stop { .. })));

        let effect = state.on_input(ptt_input(true), now + Duration::from_millis(200));
        assert!(effect.is_none());

        state.on_cancel(false);
        assert_eq!(
            state.stage,
            Stage::Processing,
            "cancel must not reset mid-processing — the pipeline still finishes"
        );

        let effect = state.on_processing_finished();
        assert!(
            effect.is_none(),
            "cancelled session must not spawn a deferred recording"
        );
        assert_eq!(state.stage, Stage::Idle);
    }

    fn toggle_input(external: bool) -> InputEvent {
        toggle_input_for(BINDING, external)
    }

    fn toggle_input_for(binding_id: &str, external: bool) -> InputEvent {
        InputEvent {
            binding_id: binding_id.to_string(),
            hotkey_string: binding_id.to_string(),
            is_pressed: true,
            mode: ShortcutActivation::Toggle,
            hold_threshold: Duration::ZERO,
            external,
        }
    }

    /// Start and stop one toggle recording so the machine sits in `Processing`.
    fn drive_into_processing(state: &mut CoordinatorState, now: Instant) {
        let effect = state.on_input(toggle_input(true), now);
        assert!(matches!(effect, Some(Effect::Start { .. })));
        let effect = state.on_input(toggle_input(true), now + Duration::from_millis(100));
        assert!(matches!(effect, Some(Effect::Stop { .. })));
        assert_eq!(state.stage, Stage::Processing);
    }

    const OTHER_BINDING: &str = "transcribe_with_post_process";

    /// Only one press can be pending. Once a binding has claimed it, a toggle
    /// for a different binding is ignored (as it is while recording) instead of
    /// replacing the remembered press, so the pending binding's parity holds:
    /// two transcribe toggles still net to no-op.
    #[test]
    fn different_binding_does_not_replace_pending_press() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        drive_into_processing(&mut state, now);

        let at = |ms| now + Duration::from_millis(ms);
        assert!(state.on_input(toggle_input(true), at(200)).is_none());
        assert!(state
            .on_input(toggle_input_for(OTHER_BINDING, true), at(300))
            .is_none());
        assert!(state.on_input(toggle_input(true), at(400)).is_none());

        let effect = state.on_processing_finished();
        assert!(
            effect.is_none(),
            "two transcribe toggles net to no-op; the ignored post-process toggle must not start"
        );
        assert_eq!(state.stage, Stage::Idle);
    }

    /// The binding that claimed the pending press is the one that starts on
    /// drain, regardless of other bindings toggled in between.
    #[test]
    fn drain_starts_the_pending_binding_not_a_later_one() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        drive_into_processing(&mut state, now);

        let at = |ms| now + Duration::from_millis(ms);
        assert!(state.on_input(toggle_input(true), at(200)).is_none());
        assert!(state
            .on_input(toggle_input_for(OTHER_BINDING, true), at(300))
            .is_none());

        match state.on_processing_finished() {
            Some(Effect::Start { binding_id, .. }) => assert_eq!(binding_id, BINDING),
            other => panic!("expected Start for '{BINDING}', got {other:?}"),
        }
    }

    /// External triggers fire on every edge by design (e.g. SIGUSR2 sent on
    /// both key press and release). Two edges inside the debounce window must
    /// both be honoured, or the parity desyncs and recording wedges on.
    #[test]
    fn external_edges_inside_debounce_window_are_not_dropped() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();

        let effect = state.on_input(toggle_input(true), now);
        assert!(matches!(effect, Some(Effect::Start { .. })));

        let effect = state.on_input(toggle_input(true), now + Duration::from_millis(5));
        assert!(
            matches!(effect, Some(Effect::Stop { .. })),
            "second external edge inside DEBOUNCE must stop the recording"
        );
        assert_eq!(state.stage, Stage::Processing);
    }

    /// Physical keyboard presses keep the debounce: a repeat inside the window
    /// is still dropped and recording stays active.
    #[test]
    fn keyboard_press_inside_debounce_window_is_still_dropped() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();

        let effect = state.on_input(toggle_input(false), now);
        assert!(matches!(effect, Some(Effect::Start { .. })));

        let effect = state.on_input(toggle_input(false), now + Duration::from_millis(5));
        assert!(
            effect.is_none(),
            "keyboard repeat inside DEBOUNCE must be debounced"
        );
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));
    }

    /// If the start effect fails to begin recording (e.g. microphone access
    /// denied), the optimistic transition rolls back to idle.
    #[test]
    fn failed_start_rolls_back_to_idle() {
        let mut state = CoordinatorState::new();

        let effect = state.on_input(ptt_input(true), Instant::now());
        assert!(matches!(effect, Some(Effect::Start { .. })));

        state.on_start_result(BINDING, None);
        assert_eq!(state.stage, Stage::Idle);
    }

    // ---------------------------------------------------------------------
    // Hold-or-toggle (the combined mode from #147) and the two legacy modes,
    // driven through the real machine on a synthetic clock. Recording starts
    // on key-down in every mode; the tests pin how each mode ends it.
    // ---------------------------------------------------------------------

    const HOLD_THRESHOLD: Duration = Duration::from_millis(300);

    fn input(mode: ShortcutActivation, is_pressed: bool) -> InputEvent {
        InputEvent {
            binding_id: BINDING.to_string(),
            hotkey_string: BINDING.to_string(),
            is_pressed,
            mode,
            hold_threshold: HOLD_THRESHOLD,
            external: false,
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Hold-or-toggle: a key held past the threshold is push-to-talk — the
    /// (deferred) release stops recording.
    #[test]
    fn hold_or_toggle_long_hold_stops_on_release() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(input(mode, true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.on_input(input(mode, false), t0 + ms(800)).is_none());
        assert!(
            matches!(state.on_grace_expired(), Some(Effect::Stop { .. })),
            "an 800ms hold must stop when its release grace elapses"
        );
        assert_eq!(state.stage, Stage::Processing);
    }

    /// Hold-or-toggle: a tap keeps recording (locked on); the next press stops.
    #[test]
    fn hold_or_toggle_tap_locks_recording_until_next_press() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(input(mode, true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.on_input(input(mode, false), t0 + ms(120)).is_none());
        assert!(
            state.on_grace_expired().is_none(),
            "a 120ms tap must not stop recording"
        );
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));
        assert!(state.is_locked());

        // Seconds later the user presses again to finish.
        assert!(matches!(
            state.on_input(input(mode, true), t0 + ms(5000)),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(state.stage, Stage::Processing);
        // The release of that stopping press lands in the busy window and is
        // ignored, so nothing is remembered for the drain.
        assert!(state.on_input(input(mode, false), t0 + ms(5080)).is_none());
        assert!(state.on_processing_finished().is_none());
        assert_eq!(state.stage, Stage::Idle);
    }

    /// Hold-or-toggle: a locked session ignores stray releases — only a press
    /// ends it.
    #[test]
    fn hold_or_toggle_locked_session_ignores_release() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        state.on_input(input(mode, true), t0);
        state.on_input(input(mode, false), t0 + ms(100));
        assert!(state.on_grace_expired().is_none());
        assert!(state.is_locked());

        assert!(state.on_input(input(mode, false), t0 + ms(900)).is_none());
        assert!(
            state.grace_deadline().is_none(),
            "no release may be deferred once locked"
        );
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));
    }

    /// Hold-or-toggle: while the key is genuinely held, extra presses do not
    /// stop the recording (that is the release's job).
    #[test]
    fn hold_or_toggle_press_while_held_is_ignored() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        state.on_input(input(mode, true), t0);
        assert!(state.on_input(input(mode, true), t0 + ms(400)).is_none());
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));
        assert!(!state.is_locked());
    }

    /// Hold-or-toggle under X11 auto-repeat: the synthesized release/press
    /// pairs must not be misread as taps. The hold is measured from the
    /// original key-down to the genuine key-up.
    #[test]
    fn hold_or_toggle_autorepeat_burst_is_one_long_hold() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        let mut clock = t0;

        assert!(matches!(
            state.on_input(input(mode, true), clock),
            Some(Effect::Start { .. })
        ));
        // ~600ms of auto-repeat pairs a few ms apart.
        for _ in 0..60 {
            clock += ms(5);
            assert!(state.on_input(input(mode, false), clock).is_none());
            clock += ms(5);
            assert!(state.on_input(input(mode, true), clock).is_none());
            assert!(
                state.grace_deadline().is_none(),
                "auto-repeat press must cancel the deferred release"
            );
        }
        assert!(!state.is_locked(), "no tap may be classified mid-burst");

        clock += ms(5);
        assert!(state.on_input(input(mode, false), clock).is_none());
        assert!(
            matches!(state.on_grace_expired(), Some(Effect::Stop { .. })),
            "the genuine release after a ~600ms hold must stop recording"
        );
    }

    /// Hold-or-toggle: a press remembered during the busy window is measured
    /// from the real key-down, so a hold that straddles the drain still counts
    /// as a hold when it is released shortly after recording actually starts.
    #[test]
    fn hold_or_toggle_remembered_press_measures_hold_from_real_key_down() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        // Previous session: hold, release, stop → Processing.
        state.on_input(input(mode, true), t0);
        state.on_input(input(mode, false), t0 + ms(800));
        assert!(matches!(
            state.on_grace_expired(),
            Some(Effect::Stop { .. })
        ));

        // Pressed again while busy; still held when the pipeline drains 700ms later.
        assert!(state.on_input(input(mode, true), t0 + ms(1000)).is_none());
        assert!(matches!(
            state.on_processing_finished(),
            Some(Effect::Start { .. })
        ));
        // Released 100ms after recording began — but 800ms after key-down.
        assert!(state.on_input(input(mode, false), t0 + ms(1800)).is_none());
        assert!(
            matches!(state.on_grace_expired(), Some(Effect::Stop { .. })),
            "held 800ms overall: must stop, not lock"
        );
    }

    /// Toggle: releases never stop, the next press does. (Toggle is the
    /// combined machine with the session locked from the start.)
    #[test]
    fn toggle_mode_ignores_release_and_stops_on_next_press() {
        let mode = ShortcutActivation::Toggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(input(mode, true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.is_locked());
        assert!(state.on_input(input(mode, false), t0 + ms(100)).is_none());
        assert!(
            state.grace_deadline().is_none(),
            "toggle never defers releases"
        );
        assert!(state.on_input(input(mode, false), t0 + ms(3000)).is_none());
        assert!(matches!(
            state.on_input(input(mode, true), t0 + ms(4000)),
            Some(Effect::Stop { .. })
        ));
    }

    /// Push-to-talk: even a very short press stops on release — there is no
    /// tap-to-lock in this mode (hold threshold of zero).
    #[test]
    fn push_to_talk_short_press_still_stops_on_release() {
        let mode = ShortcutActivation::PushToTalk;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(input(mode, true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.on_input(input(mode, false), t0 + ms(40)).is_none());
        assert!(matches!(
            state.on_grace_expired(),
            Some(Effect::Stop { .. })
        ));
    }

    /// Cancel (Escape) during a locked hold-or-toggle session resets cleanly so
    /// the next press starts a fresh recording rather than stopping a dead one.
    #[test]
    fn hold_or_toggle_cancel_clears_locked_session() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        state.on_input(input(mode, true), t0);
        state.on_input(input(mode, false), t0 + ms(100));
        assert!(state.on_grace_expired().is_none());
        assert!(state.is_locked());

        state.on_cancel(true);
        assert_eq!(state.stage, Stage::Idle);
        assert!(!state.is_locked());
        assert!(matches!(
            state.on_input(input(mode, true), t0 + ms(2000)),
            Some(Effect::Start { .. })
        ));
    }

    /// Switching to toggle while an unlocked hold recording is running must not
    /// strand it: in toggle mode a press always stops.
    #[test]
    fn toggle_press_stops_recording_started_as_hold() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        state.on_input(input(ShortcutActivation::HoldOrToggle, true), t0);
        assert!(!state.is_locked());
        assert!(matches!(
            state.on_input(input(ShortcutActivation::Toggle, true), t0 + ms(2000)),
            Some(Effect::Stop { .. })
        ));
    }

    // Hold-vs-tap classification while the previous transcription is busy.
    fn hold_or_toggle_into_processing(state: &mut CoordinatorState, t0: Instant) {
        let mode = ShortcutActivation::HoldOrToggle;
        assert!(matches!(
            state.on_input(input(mode, true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.on_input(input(mode, false), t0 + ms(800)).is_none());
        assert!(matches!(
            state.on_grace_expired(),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(state.stage, Stage::Processing);
    }

    #[test]
    fn hold_or_toggle_tap_during_processing_queues_locked_start() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        hold_or_toggle_into_processing(&mut state, t0);

        assert!(state.on_input(input(mode, true), t0 + ms(1000)).is_none());
        assert!(state.on_input(input(mode, false), t0 + ms(1100)).is_none());
        assert!(state.on_grace_expired().is_none());
        assert!(state.is_locked(), "a busy tap should queue a locked start");
        assert!(matches!(
            state.on_processing_finished(),
            Some(Effect::Start { .. })
        ));
        assert!(state.is_locked());
    }

    #[test]
    fn hold_or_toggle_completed_hold_during_processing_nets_noop() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        hold_or_toggle_into_processing(&mut state, t0);

        assert!(state.on_input(input(mode, true), t0 + ms(1000)).is_none());
        assert!(state.on_input(input(mode, false), t0 + ms(1600)).is_none());
        assert!(state.on_grace_expired().is_none());
        assert!(!state.is_locked());

        assert!(
            state.on_processing_finished().is_none(),
            "a 600ms hold that ended before the drain has nothing left to start"
        );
        assert_eq!(state.stage, Stage::Idle);
    }

    #[test]
    fn hold_or_toggle_two_taps_during_processing_net_noop() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        hold_or_toggle_into_processing(&mut state, t0);

        assert!(state.on_input(input(mode, true), t0 + ms(1000)).is_none());
        assert!(state.on_input(input(mode, false), t0 + ms(1100)).is_none());
        assert!(state.on_grace_expired().is_none());
        assert!(state.is_locked());

        assert!(state.on_input(input(mode, true), t0 + ms(1500)).is_none());
        assert!(
            !state.is_locked(),
            "the second tap's press forgets the queued tap"
        );
        assert!(state.on_input(input(mode, false), t0 + ms(1600)).is_none());
        assert!(state.grace_deadline().is_none());

        assert!(state.on_processing_finished().is_none());
        assert_eq!(state.stage, Stage::Idle);
    }

    #[test]
    fn ptt_tap_inside_busy_window_nets_noop() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        hold_or_toggle_into_processing(&mut state, t0);

        assert!(state.on_input(ptt_input(true), t0 + ms(1000)).is_none());
        assert!(state.on_input(ptt_input(false), t0 + ms(1040)).is_none());
        assert!(state.on_grace_expired().is_none());
        assert!(state.on_processing_finished().is_none());
    }

    /// The pipeline drains inside the 50ms grace of a busy tap: recording
    /// starts first (unlocked, from the real key-down), and the grace then
    /// resolves against the live recording, locking it as the tap it was.
    #[test]
    fn hold_or_toggle_drain_inside_busy_release_grace_still_classifies_tap() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        hold_or_toggle_into_processing(&mut state, t0);

        assert!(state.on_input(input(mode, true), t0 + ms(1000)).is_none());
        assert!(state.on_input(input(mode, false), t0 + ms(1100)).is_none());
        assert!(matches!(
            state.on_processing_finished(),
            Some(Effect::Start { .. })
        ));
        assert!(!state.is_locked());

        assert!(state.on_grace_expired().is_none());
        assert!(state.is_locked(), "the deferred 100ms release is a tap");
    }

    /// X11 auto-repeat while busy, key still held at the drain: recording
    /// starts measured from the first press, not from the last synthesized
    /// press before the drain. Released 400ms after the real key-down but
    /// only ~100ms after the drain — a hold, so it must stop rather than lock.
    #[test]
    fn hold_or_toggle_autorepeat_burst_straddling_drain_measures_from_first_press() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        hold_or_toggle_into_processing(&mut state, t0);

        let mut clock = t0 + ms(1000);
        assert!(state.on_input(input(mode, true), clock).is_none());
        for _ in 0..30 {
            clock += ms(5);
            assert!(state.on_input(input(mode, false), clock).is_none());
            clock += ms(5);
            assert!(state.on_input(input(mode, true), clock).is_none());
            assert!(state.grace_deadline().is_none());
        }

        // Drain at ~t0 + 1300ms with the key still down.
        assert!(matches!(
            state.on_processing_finished(),
            Some(Effect::Start { .. })
        ));
        assert!(!state.is_locked());

        for _ in 0..10 {
            clock += ms(5);
            assert!(state.on_input(input(mode, false), clock).is_none());
            clock += ms(5);
            assert!(state.on_input(input(mode, true), clock).is_none());
        }
        assert_eq!(clock, t0 + ms(1400));
        assert!(state.on_input(input(mode, false), clock).is_none());
        assert!(
            matches!(state.on_grace_expired(), Some(Effect::Stop { .. })),
            "held 400ms since the real key-down: must stop, not lock"
        );
        assert_eq!(state.stage, Stage::Processing);
    }
}
