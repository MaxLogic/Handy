//! Production-path timing for the stop-to-output dictation pipeline.
//!
//! One [`OperationTimer`] follows a single accepted stop event until the text
//! is pasted or the operation ends some other way. When it is dropped by its
//! last holder it emits one [`TimingRecord`] as a JSON line. Records carry only
//! numbers and enums: never transcript, selection, prompt or credential text.
//!
//! Recording is opt-in: set `HANDY_DICTATION_TIMING_FILE` to a file path
//! before launching the app (see `docs/dictation-timing.md`). Without it no
//! file is written. The hot path only reads the monotonic clock, bumps a few
//! atomics and does one non-blocking `try_send` into a bounded channel; a
//! dedicated thread owns all file I/O. If the channel is full the record is
//! dropped and counted (`dropped_records`, visible as a `seq` gap).
//!
//! Stage boundaries (all `std::time::Instant` durations, microseconds):
//! - `stop_sync`: `stop()` entry until the background task is spawned
//! - `task_sched`: spawn until the task body starts running
//! - `audio_finalize`: `stop_recording`, i.e. samples retrieved
//! - `stt`: stream finalize / batch transcribe, minus `text_cleanup`. Includes
//!   engine queue wait and any model load it had to wait for; `cold` and
//!   `engine_pending_at_stt` tell those cases apart from a warm run.
//! - `text_cleanup`: deterministic text post-processing done inside the STT
//!   calls (script conversion, custom-word correction, filler removal)
//! - `wav_wait`: residual wait for the concurrent WAV save after STT
//! - `llm`: LLM post-processing step (no work in raw mode)
//! - `history`: history entry save
//! - `main_sched`: `run_on_main_thread` call until the closure starts
//! - `paste`: `utils::paste` (includes its internal settle/restore waits)
//!
//! The stages are sequential and disjoint, so their sum must not exceed
//! `total_us` (stop accepted until output completed or refused).

use log::{debug, error, info, warn};
use serde::Serialize;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Version of the JSONL record layout; the summarizer rejects other versions.
pub const SCHEMA_VERSION: u32 = 1;
/// Environment variable naming the JSONL output file. Unset means disabled.
pub const ENV_TIMING_FILE: &str = "HANDY_DICTATION_TIMING_FILE";
/// Environment variable naming the workload (for example the spoken language,
/// `en` or `pl`). Recorded on every operation of this process.
pub const ENV_TIMING_LABEL: &str = "HANDY_DICTATION_TIMING_LABEL";
const MAX_LABEL_LEN: usize = 32;
/// Records buffered between the pipeline and the writer thread.
const CHANNEL_CAPACITY: usize = 64;
/// Most engine commands that may be waiting when STT starts (a stream worker
/// plus one queued command); more means unbounded pending work.
pub const MAX_ENGINE_PENDING: u32 = 2;
/// Slack when checking that stage durations fit inside the total.
const STAGE_SUM_TOLERANCE_US: u64 = 1_000;
/// The recorder delivers 16 kHz mono PCM to the pipeline.
const SAMPLE_RATE_HZ: u64 = 16_000;

static MODEL_LOAD_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static WORKER_SPAWNS: AtomicU64 = AtomicU64::new(0);
static WORKER_PID: AtomicU32 = AtomicU32::new(0);
static LLM_REQUESTS: AtomicU64 = AtomicU64::new(0);
static ACTIVE_OPS: AtomicU32 = AtomicU32::new(0);
static ENGINE_PENDING: AtomicU32 = AtomicU32::new(0);
static LABEL: OnceLock<Option<String>> = OnceLock::new();
static NEXT_OP_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);
static PENDING_START: Mutex<Option<StartSnapshot>> = Mutex::new(None);
static SINK: OnceLock<Option<Sink>> = OnceLock::new();
static SESSION: OnceLock<u64> = OnceLock::new();

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn session_id() -> u64 {
    *SESSION.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    })
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// Process-wide counters fed by the model and engine code
// ---------------------------------------------------------------------------

/// A model load was requested (transcribe-cpp or ONNX).
pub fn note_model_load_attempt() {
    MODEL_LOAD_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
}

/// A transcription worker process was started.
pub fn note_worker_spawned(pid: u32) {
    WORKER_SPAWNS.fetch_add(1, Ordering::Relaxed);
    WORKER_PID.store(pid, Ordering::Relaxed);
}

/// An LLM post-processing request is about to be sent.
pub fn note_llm_request() {
    LLM_REQUESTS.fetch_add(1, Ordering::Relaxed);
}

/// Counts one outstanding command for as long as it lives.
pub struct PendingGuard<'a>(&'a AtomicU32);

impl<'a> PendingGuard<'a> {
    fn enter(counter: &'a AtomicU32) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Counts one outstanding engine command until the guard is dropped.
pub fn engine_pending_guard() -> PendingGuard<'static> {
    PendingGuard::enter(&ENGINE_PENDING)
}

/// Engine commands (load, unload, transcribe, ...) currently outstanding.
/// Only commands going through the transcribe-cpp supervisor are counted.
pub fn engine_pending() -> u32 {
    ENGINE_PENDING.load(Ordering::Relaxed)
}

thread_local! {
    static TEXT_CLEANUP_US: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Runs `f` and adds its duration to this thread's deterministic text-cleanup
/// total, which [`OperationTimer::add_stt`] later separates from STT time.
pub fn time_text_cleanup<T>(f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let value = f();
    let spent = micros(started.elapsed());
    TEXT_CLEANUP_US.with(|c| c.set(c.get().saturating_add(spent)));
    value
}

/// Clears this thread's text-cleanup total; call right before STT starts.
pub fn reset_text_cleanup() {
    TEXT_CLEANUP_US.with(|c| c.set(0));
}

fn take_text_cleanup_us() -> u64 {
    TEXT_CLEANUP_US.with(|c| c.replace(0))
}

/// Point-in-time copy of the process-wide counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub model_loads: u64,
    pub worker_spawns: u64,
    pub worker_pid: u32,
    pub llm_requests: u64,
}

pub fn counters() -> Counters {
    Counters {
        model_loads: MODEL_LOAD_ATTEMPTS.load(Ordering::Relaxed),
        worker_spawns: WORKER_SPAWNS.load(Ordering::Relaxed),
        worker_pid: WORKER_PID.load(Ordering::Relaxed),
        llm_requests: LLM_REQUESTS.load(Ordering::Relaxed),
    }
}

// ---------------------------------------------------------------------------
// Record types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Transcription pasted without LLM cleanup.
    Raw,
    /// Transcription followed by the existing LLM post-processing route.
    Cleanup,
}

impl Mode {
    pub fn from_post_process(post_process: bool) -> Self {
        if post_process {
            Mode::Cleanup
        } else {
            Mode::Raw
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Text was handed to the paste path and it reported success.
    Pasted,
    /// STT and cleanup produced no text to insert.
    EmptyOutput,
    PasteError,
    /// The paste could not be scheduled on the main thread.
    MainThreadError,
    TranscriptionError,
    Cancelled,
    /// The recorder returned no samples.
    NoSamples,
    /// The recorder returned an empty buffer.
    EmptyAudio,
    /// The operation ended without reaching a terminal state.
    Aborted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SttPath {
    /// Text came from finalizing the live stream.
    StreamFinalize,
    /// Text came from a batch transcription of the captured audio.
    Batch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    StopSync,
    TaskSched,
    AudioFinalize,
    Stt,
    WavWait,
    TextCleanup,
    Llm,
    History,
    MainSched,
    Paste,
}

/// Per-stage durations in microseconds; absent when the stage was not reached.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Stages {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sync_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_sched_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_finalize_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stt_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wav_wait_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_cleanup_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_sched_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paste_us: Option<u64>,
}

impl Stages {
    fn slot(&mut self, stage: Stage) -> &mut Option<u64> {
        match stage {
            Stage::StopSync => &mut self.stop_sync_us,
            Stage::TaskSched => &mut self.task_sched_us,
            Stage::AudioFinalize => &mut self.audio_finalize_us,
            Stage::Stt => &mut self.stt_us,
            Stage::WavWait => &mut self.wav_wait_us,
            Stage::TextCleanup => &mut self.text_cleanup_us,
            Stage::Llm => &mut self.llm_us,
            Stage::History => &mut self.history_us,
            Stage::MainSched => &mut self.main_sched_us,
            Stage::Paste => &mut self.paste_us,
        }
    }

    /// Sum of the stages that were reached.
    pub fn sum_us(&self) -> u64 {
        [
            self.stop_sync_us,
            self.task_sched_us,
            self.audio_finalize_us,
            self.stt_us,
            self.wav_wait_us,
            self.text_cleanup_us,
            self.llm_us,
            self.history_us,
            self.main_sched_us,
            self.paste_us,
        ]
        .into_iter()
        .flatten()
        .fold(0u64, u64::saturating_add)
    }
}

/// One JSONL line. Contains no user text.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TimingRecord {
    pub schema: u32,
    /// Process start (unix ms); disambiguates `seq` across app restarts.
    pub session: u64,
    /// Gapless per session unless a record was dropped or lost.
    pub seq: u64,
    /// Correlates with the `op=` field in the app log.
    pub op_id: u64,
    pub mode: Mode,
    pub outcome: Outcome,
    /// The configured language setting (for example `en`), not content.
    pub language: String,
    /// Workload label from `HANDY_DICTATION_TIMING_LABEL`, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub build: &'static str,
    pub app_version: &'static str,
    /// True when the model was not resident when the recording started.
    pub cold: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stt_path: Option<SttPath>,
    pub stages: Stages,
    /// Accepted stop until completion or refusal of the output.
    pub total_us: u64,
    pub model_loads_delta: u64,
    pub worker_spawns_delta: u64,
    pub worker_pid_before: u32,
    pub worker_pid_after: u32,
    pub llm_requests: u64,
    pub paste_calls: u32,
    /// Operations in flight when this stop was accepted (including itself).
    pub active_ops: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_pending_at_stt: Option<u32>,
    /// Records dropped before this one because the writer fell behind.
    pub dropped_records: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invariant {
    /// More than one operation active at once.
    MultipleActiveOperations,
    /// Model reloaded or worker restarted during a warm utterance.
    ReloadDuringWarmUtterance,
    /// An LLM request was made in raw mode.
    LlmRequestInRawMode,
    /// More than one paste for one operation.
    DuplicatePaste,
    /// More engine commands pending than the bound allows.
    EnginePendingExceeded,
    /// Stage durations add up to more than the total.
    StageSumExceedsTotal,
}

/// Invariants this record breaks. The TypeScript summarizer mirrors these
/// checks; keep them in sync.
pub fn violations(record: &TimingRecord) -> Vec<Invariant> {
    let mut found = Vec::new();
    if record.active_ops > 1 {
        found.push(Invariant::MultipleActiveOperations);
    }
    if !record.cold && (record.model_loads_delta > 0 || record.worker_spawns_delta > 0) {
        found.push(Invariant::ReloadDuringWarmUtterance);
    }
    if record.mode == Mode::Raw && record.llm_requests > 0 {
        found.push(Invariant::LlmRequestInRawMode);
    }
    if record.paste_calls > 1 {
        found.push(Invariant::DuplicatePaste);
    }
    if record
        .engine_pending_at_stt
        .is_some_and(|pending| pending > MAX_ENGINE_PENDING)
    {
        found.push(Invariant::EnginePendingExceeded);
    }
    if record.stages.sum_us() > record.total_us.saturating_add(STAGE_SUM_TOLERANCE_US) {
        found.push(Invariant::StageSumExceedsTotal);
    }
    found
}

// ---------------------------------------------------------------------------
// Operation timer
// ---------------------------------------------------------------------------

struct StartSnapshot {
    op_id: u64,
    counters: Counters,
    model_resident: bool,
}

/// Call at the start of a recording, before the model load is kicked off.
/// `model_resident` is whether the model was already loaded. Cancelled
/// recordings leave a stale snapshot that the next start overwrites.
pub fn note_recording_start(model_resident: bool) {
    let op_id = NEXT_OP_ID.fetch_add(1, Ordering::Relaxed);
    debug!("dictation timing: op={op_id} recording started (model_resident={model_resident})");
    *lock(&PENDING_START) = Some(StartSnapshot {
        op_id,
        counters: counters(),
        model_resident,
    });
}

struct Inner {
    started: Instant,
    op_id: u64,
    mode: Mode,
    language: String,
    start_counters: Counters,
    model_resident: bool,
    active_ops: u32,
    /// Whether this holds a slot in the global active-operation count.
    holds_active_slot: bool,
    audio_ms: Option<u64>,
    stt_path: Option<SttPath>,
    stages: Stages,
    paste_calls: u32,
    engine_pending_at_stt: Option<u32>,
    finished: Option<Finished>,
}

struct Finished {
    outcome: Outcome,
    total_us: u64,
    counters: Counters,
}

impl Inner {
    fn new(
        started: Instant,
        op_id: u64,
        mode: Mode,
        language: String,
        start_counters: Counters,
        model_resident: bool,
        active_ops: u32,
    ) -> Self {
        Self {
            started,
            op_id,
            mode,
            language,
            start_counters,
            model_resident,
            active_ops,
            holds_active_slot: false,
            audio_ms: None,
            stt_path: None,
            stages: Stages::default(),
            paste_calls: 0,
            engine_pending_at_stt: None,
            finished: None,
        }
    }

    /// Records the first terminal outcome; later calls are ignored so a
    /// follow-up cleanup path cannot overwrite what actually happened.
    fn finish(&mut self, outcome: Outcome, total_us: u64, counters: Counters) {
        if self.finished.is_none() {
            self.finished = Some(Finished {
                outcome,
                total_us,
                counters,
            });
        }
    }

    fn build_record(&self, fallback_total_us: u64, fallback_counters: Counters) -> TimingRecord {
        let (outcome, total_us, end) = match &self.finished {
            Some(f) => (f.outcome, f.total_us, f.counters),
            None => (Outcome::Aborted, fallback_total_us, fallback_counters),
        };
        TimingRecord {
            schema: SCHEMA_VERSION,
            session: 0,
            seq: 0,
            op_id: self.op_id,
            mode: self.mode,
            outcome,
            language: self.language.clone(),
            label: LABEL.get().cloned().flatten(),
            build: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            app_version: env!("CARGO_PKG_VERSION"),
            cold: !self.model_resident,
            audio_ms: self.audio_ms,
            stt_path: self.stt_path,
            stages: self.stages.clone(),
            total_us,
            model_loads_delta: end
                .model_loads
                .saturating_sub(self.start_counters.model_loads),
            worker_spawns_delta: end
                .worker_spawns
                .saturating_sub(self.start_counters.worker_spawns),
            worker_pid_before: self.start_counters.worker_pid,
            worker_pid_after: end.worker_pid,
            llm_requests: end
                .llm_requests
                .saturating_sub(self.start_counters.llm_requests),
            paste_calls: self.paste_calls,
            active_ops: self.active_ops,
            engine_pending_at_stt: self.engine_pending_at_stt,
            dropped_records: 0,
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if self.holds_active_slot {
            ACTIVE_OPS.fetch_sub(1, Ordering::Relaxed);
        }
        let record = self.build_record(micros(self.started.elapsed()), counters());
        debug!(
            "dictation timing: op={} mode={:?} outcome={:?} total={}us",
            record.op_id, record.mode, record.outcome, record.total_us
        );
        emit(record);
    }
}

/// Follows one accepted stop event. Clone it into each task that continues the
/// operation; the record is emitted when the last clone is dropped.
#[derive(Clone)]
pub struct OperationTimer(Arc<Mutex<Inner>>);

impl OperationTimer {
    /// Start timing at the accepted stop event. Pairs with the snapshot from
    /// [`note_recording_start`]; without one (for example a stop that arrives
    /// first) the counters are read now and the run counts as warm.
    pub fn begin_stop(mode: Mode, language: String) -> Self {
        let started = Instant::now();
        let snapshot = lock(&PENDING_START).take();
        let (op_id, start_counters, model_resident) = match snapshot {
            Some(s) => (s.op_id, s.counters, s.model_resident),
            None => (NEXT_OP_ID.fetch_add(1, Ordering::Relaxed), counters(), true),
        };
        let active_ops = ACTIVE_OPS.fetch_add(1, Ordering::Relaxed) + 1;
        let mut inner = Inner::new(
            started,
            op_id,
            mode,
            language,
            start_counters,
            model_resident,
            active_ops,
        );
        inner.holds_active_slot = true;
        debug!("dictation timing: op={op_id} stop accepted (active_ops={active_ops})");
        Self(Arc::new(Mutex::new(inner)))
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        lock(&self.0)
    }

    /// Adds `elapsed` to a stage.
    pub fn add(&self, stage: Stage, elapsed: Duration) {
        let mut inner = self.inner();
        let slot = inner.stages.slot(stage);
        *slot = Some(slot.unwrap_or(0).saturating_add(micros(elapsed)));
    }

    /// Records the STT call. `total` covers the whole call; the deterministic
    /// text cleanup done inside it on this thread (see [`time_text_cleanup`])
    /// is reported as its own stage and excluded from `stt`.
    pub fn add_stt(&self, total: Duration) {
        let cleanup_us = take_text_cleanup_us();
        let mut inner = self.inner();
        let stt = micros(total).saturating_sub(cleanup_us);
        let slot = inner.stages.slot(Stage::Stt);
        *slot = Some(slot.unwrap_or(0).saturating_add(stt));
        let slot = inner.stages.slot(Stage::TextCleanup);
        *slot = Some(slot.unwrap_or(0).saturating_add(cleanup_us));
    }

    /// Adds the time since `since` to a stage.
    pub fn add_since(&self, stage: Stage, since: Instant) {
        self.add(stage, since.elapsed());
    }

    pub fn set_language(&self, language: String) {
        self.inner().language = language;
    }

    pub fn set_audio_samples(&self, sample_count: usize) {
        self.inner().audio_ms = Some(sample_count as u64 * 1000 / SAMPLE_RATE_HZ);
    }

    pub fn set_stt_path(&self, path: SttPath) {
        self.inner().stt_path = Some(path);
    }

    /// Samples the engine queue just before STT begins.
    pub fn note_engine_pending(&self) {
        self.inner().engine_pending_at_stt = Some(engine_pending());
    }

    pub fn note_paste_call(&self) {
        let mut inner = self.inner();
        inner.paste_calls = inner.paste_calls.saturating_add(1);
    }

    /// Marks the operation complete. The first outcome wins.
    pub fn finish(&self, outcome: Outcome) {
        let mut inner = self.inner();
        let total_us = micros(inner.started.elapsed());
        inner.finish(outcome, total_us, counters());
    }
}

// ---------------------------------------------------------------------------
// Bounded sink and writer thread
// ---------------------------------------------------------------------------

/// Written once when the app exits cleanly. Lets the summarizer detect a
/// lost last record (and a crash, which leaves no marker).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionEnd {
    kind: &'static str,
    schema: u32,
    session: u64,
    /// Records assigned a `seq` this session, including any dropped.
    records: u64,
    dropped_records: u64,
}

enum Message {
    Record(TimingRecord),
    End(SessionEnd, mpsc::SyncSender<()>),
}

struct Sink {
    tx: mpsc::SyncSender<Message>,
    dropped: AtomicU64,
    drop_logged: AtomicBool,
}

impl Sink {
    fn with_capacity(capacity: usize) -> (Self, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        (
            Self {
                tx,
                dropped: AtomicU64::new(0),
                drop_logged: AtomicBool::new(false),
            },
            rx,
        )
    }

    /// Never blocks. A full or closed channel drops the record and counts it.
    fn enqueue(&self, record: TimingRecord) {
        if self.tx.try_send(Message::Record(record)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            if !self.drop_logged.swap(true, Ordering::Relaxed) {
                warn!(
                    "dictation timing: record dropped because the writer is behind or stopped; \
                     further drops are counted in dropped_records"
                );
            }
        }
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

fn emit(mut record: TimingRecord) {
    #[cfg(test)]
    lock(&tests::CAPTURED).push(record.clone());
    let Some(Some(sink)) = SINK.get() else {
        return;
    };
    for violated in violations(&record) {
        warn!(
            "dictation timing: op={} violated invariant {:?}",
            record.op_id, violated
        );
    }
    record.session = session_id();
    record.seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    record.dropped_records = sink.dropped();
    sink.enqueue(record);
}

/// Writes each record as one JSON line and flushes. A failing write is
/// reported through `on_first_error` once, then counted; later records are
/// still attempted. Returns `(written, failed)`.
fn run_writer<W: Write>(
    messages: mpsc::Receiver<Message>,
    mut out: W,
    mut on_first_error: impl FnMut(&str),
) -> (u64, u64) {
    let (mut written, mut failed) = (0u64, 0u64);
    let mut reported = false;
    for message in messages {
        let (serialized, done) = match &message {
            Message::Record(record) => (serde_json::to_string(record), None),
            Message::End(end, done) => (serde_json::to_string(end), Some(done)),
        };
        let result = serialized.map_err(|e| e.to_string()).and_then(|line| {
            writeln!(out, "{line}")
                .and_then(|()| out.flush())
                .map_err(|e| e.to_string())
        });
        match result {
            Ok(()) => written += 1,
            Err(message) => {
                failed += 1;
                if !reported {
                    reported = true;
                    on_first_error(&message);
                }
            }
        }
        if let Some(done) = done {
            let _ = done.try_send(());
        }
    }
    (written, failed)
}

/// Keeps only `[A-Za-z0-9_.-]` and at most 32 characters.
fn sanitize_label(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .take(MAX_LABEL_LEN)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Writes the session-end marker and waits briefly for it to reach the file.
/// Call when the app exits. A full queue skips the marker, which the
/// summarizer then reports as a missing end.
pub fn finish_session() {
    let Some(Some(sink)) = SINK.get() else {
        return;
    };
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let end = SessionEnd {
        kind: "session_end",
        schema: SCHEMA_VERSION,
        session: session_id(),
        records: NEXT_SEQ.load(Ordering::Relaxed).saturating_sub(1),
        dropped_records: sink.dropped(),
    };
    if sink.tx.try_send(Message::End(end, done_tx)).is_ok() {
        let _ = done_rx.recv_timeout(Duration::from_secs(1));
    }
}

/// Reads `HANDY_DICTATION_TIMING_FILE` and starts the writer thread if set.
/// Safe to call more than once; only the first call has an effect.
pub fn init() {
    LABEL.get_or_init(|| {
        let raw = std::env::var(ENV_TIMING_LABEL).ok()?;
        let label = sanitize_label(&raw);
        if label.as_deref() != Some(raw.as_str()) {
            warn!("dictation timing: {ENV_TIMING_LABEL} {raw:?} was reduced to {label:?}");
        }
        label
    });
    SINK.get_or_init(|| {
        let path = std::env::var_os(ENV_TIMING_FILE).filter(|v| !v.is_empty())?;
        let path = std::path::PathBuf::from(path);
        let (sink, rx) = Sink::with_capacity(CHANNEL_CAPACITY);
        let thread_path = path.clone();
        let spawned = std::thread::Builder::new()
            .name("dictation-timing-writer".into())
            .spawn(move || {
                let file = thread_path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| {
                        std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&thread_path)
                    });
                let report = |message: &str| {
                    error!(
                        "dictation timing: cannot write {} ({}); records are being discarded",
                        thread_path.display(),
                        message
                    );
                };
                match file {
                    Ok(file) => {
                        run_writer(rx, file, report);
                    }
                    Err(e) => {
                        report(&e.to_string());
                        // Keep draining so the pipeline never sees a full channel.
                        for _ in rx {}
                    }
                }
            });
        if let Err(e) = spawned {
            error!("dictation timing: cannot start writer thread: {e}");
            return None;
        }
        info!(
            "dictation timing: writing JSONL records to {}",
            path.display()
        );
        Some(sink)
    });
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Records handed to `emit`, captured because tests have no sink.
    pub(super) static CAPTURED: Mutex<Vec<TimingRecord>> = Mutex::new(Vec::new());
    /// Serializes tests that use the process-wide timer state.
    static GLOBAL_STATE: Mutex<()> = Mutex::new(());

    fn captured_for(op_id: u64) -> Vec<TimingRecord> {
        lock(&CAPTURED)
            .iter()
            .filter(|r| r.op_id == op_id)
            .cloned()
            .collect()
    }

    fn op_id_of(timer: &OperationTimer) -> u64 {
        timer.inner().op_id
    }

    fn counters_at(loads: u64, spawns: u64, pid: u32, llm: u64) -> Counters {
        Counters {
            model_loads: loads,
            worker_spawns: spawns,
            worker_pid: pid,
            llm_requests: llm,
        }
    }

    fn inner(mode: Mode, resident: bool) -> Inner {
        Inner::new(
            Instant::now(),
            1 << 60, // never collides with real operation ids
            mode,
            "en".to_string(),
            counters_at(2, 1, 100, 5),
            resident,
            1,
        )
    }

    fn finished_record(
        inner: &mut Inner,
        outcome: Outcome,
        total_us: u64,
        end: Counters,
    ) -> TimingRecord {
        inner.finish(outcome, total_us, end);
        inner.build_record(0, Counters::default())
    }

    #[test]
    fn clean_warm_raw_record_has_no_violations() {
        let mut op = inner(Mode::Raw, true);
        op.stages.audio_finalize_us = Some(2_000);
        op.stages.stt_us = Some(300_000);
        op.stages.paste_us = Some(40_000);
        let record = finished_record(&mut op, Outcome::Pasted, 400_000, counters_at(2, 1, 100, 5));
        assert!(!record.cold);
        assert_eq!(record.model_loads_delta, 0);
        assert_eq!(record.worker_spawns_delta, 0);
        assert_eq!(record.worker_pid_before, record.worker_pid_after);
        assert_eq!(record.llm_requests, 0);
        assert!(violations(&record).is_empty());
    }

    #[test]
    fn deltas_are_measured_against_the_start_snapshot() {
        let mut op = inner(Mode::Cleanup, false);
        let record = finished_record(&mut op, Outcome::Pasted, 10, counters_at(3, 2, 222, 6));
        assert!(record.cold);
        assert_eq!(record.model_loads_delta, 1);
        assert_eq!(record.worker_spawns_delta, 1);
        assert_eq!(record.worker_pid_before, 100);
        assert_eq!(record.worker_pid_after, 222);
        assert_eq!(record.llm_requests, 1);
        // A cold run that loads the model is expected, not a violation.
        assert!(violations(&record).is_empty());
    }

    #[test]
    fn warm_reload_and_worker_restart_are_violations() {
        let mut op = inner(Mode::Raw, true);
        let record = finished_record(&mut op, Outcome::Pasted, 10, counters_at(3, 1, 100, 5));
        assert_eq!(
            violations(&record),
            vec![Invariant::ReloadDuringWarmUtterance]
        );

        let mut op = inner(Mode::Raw, true);
        let record = finished_record(&mut op, Outcome::Pasted, 10, counters_at(2, 2, 101, 5));
        assert_eq!(
            violations(&record),
            vec![Invariant::ReloadDuringWarmUtterance]
        );
    }

    #[test]
    fn llm_request_in_raw_mode_is_a_violation_but_not_in_cleanup() {
        let mut raw = inner(Mode::Raw, true);
        let record = finished_record(&mut raw, Outcome::Pasted, 10, counters_at(2, 1, 100, 6));
        assert_eq!(violations(&record), vec![Invariant::LlmRequestInRawMode]);

        let mut cleanup = inner(Mode::Cleanup, true);
        let record = finished_record(&mut cleanup, Outcome::Pasted, 10, counters_at(2, 1, 100, 6));
        assert!(violations(&record).is_empty());
    }

    #[test]
    fn duplicate_paste_and_concurrent_operations_are_violations() {
        let mut op = inner(Mode::Raw, true);
        op.paste_calls = 2;
        op.active_ops = 2;
        let record = finished_record(&mut op, Outcome::Pasted, 10, counters_at(2, 1, 100, 5));
        assert_eq!(
            violations(&record),
            vec![
                Invariant::MultipleActiveOperations,
                Invariant::DuplicatePaste
            ]
        );
    }

    #[test]
    fn engine_pending_bound_is_inclusive() {
        let mut op = inner(Mode::Raw, true);
        op.engine_pending_at_stt = Some(MAX_ENGINE_PENDING);
        let record = finished_record(&mut op, Outcome::Pasted, 10, counters_at(2, 1, 100, 5));
        assert!(violations(&record).is_empty());

        let mut op = inner(Mode::Raw, true);
        op.engine_pending_at_stt = Some(MAX_ENGINE_PENDING + 1);
        let record = finished_record(&mut op, Outcome::Pasted, 10, counters_at(2, 1, 100, 5));
        assert_eq!(violations(&record), vec![Invariant::EnginePendingExceeded]);
    }

    #[test]
    fn stages_must_fit_inside_the_total() {
        let mut op = inner(Mode::Raw, true);
        op.stages.stt_us = Some(100_000);
        op.stages.paste_us = Some(50_000);
        // Within the tolerance: fine.
        let record = finished_record(&mut op, Outcome::Pasted, 149_500, counters_at(2, 1, 100, 5));
        assert!(violations(&record).is_empty());

        let mut op = inner(Mode::Raw, true);
        op.stages.stt_us = Some(100_000);
        op.stages.paste_us = Some(50_000);
        let record = finished_record(&mut op, Outcome::Pasted, 140_000, counters_at(2, 1, 100, 5));
        assert_eq!(violations(&record), vec![Invariant::StageSumExceedsTotal]);
    }

    #[test]
    fn timer_add_accumulates_and_only_reached_stages_count() {
        let _serial = lock(&GLOBAL_STATE);
        let timer = OperationTimer::begin_stop(Mode::Raw, "en".into());
        timer.add(Stage::Stt, Duration::from_micros(5));
        timer.add(Stage::Stt, Duration::from_micros(7));
        timer.add(Stage::Llm, Duration::from_micros(3));
        {
            let inner = timer.inner();
            assert_eq!(inner.stages.stt_us, Some(12));
            assert_eq!(inner.stages.llm_us, Some(3));
            assert_eq!(inner.stages.sum_us(), 15);
            assert_eq!(inner.stages.paste_us, None);
        }
        timer.finish(Outcome::Pasted);
    }

    #[test]
    fn stt_excludes_deterministic_text_cleanup() {
        let _serial = lock(&GLOBAL_STATE);
        let timer = OperationTimer::begin_stop(Mode::Raw, "en".into());
        reset_text_cleanup();
        let value = time_text_cleanup(|| {
            std::thread::sleep(Duration::from_millis(5));
            7
        });
        assert_eq!(value, 7);
        timer.add_stt(Duration::from_millis(50));
        {
            let inner = timer.inner();
            let cleanup = inner.stages.text_cleanup_us.unwrap();
            let stt = inner.stages.stt_us.unwrap();
            assert!(cleanup >= 5_000, "cleanup {cleanup}");
            assert_eq!(stt + cleanup, 50_000);
        }
        // The total was consumed: a second STT call starts from zero.
        timer.add_stt(Duration::from_millis(10));
        assert!(timer.inner().stages.text_cleanup_us.unwrap() >= 5_000);
        // Cleanup longer than the call cannot make STT negative.
        reset_text_cleanup();
        time_text_cleanup(|| std::thread::sleep(Duration::from_millis(3)));
        let other = OperationTimer::begin_stop(Mode::Raw, "en".into());
        other.add_stt(Duration::from_micros(1));
        assert_eq!(other.inner().stages.stt_us, Some(0));
        timer.finish(Outcome::Pasted);
        other.finish(Outcome::Pasted);
    }

    #[test]
    fn begin_stop_pairs_with_the_recording_start_snapshot() {
        let _serial = lock(&GLOBAL_STATE);
        note_recording_start(false);
        let before = counters();
        note_model_load_attempt();
        note_worker_spawned(4321);
        let timer = OperationTimer::begin_stop(Mode::Cleanup, "pl".into());
        let op_id = op_id_of(&timer);
        timer.finish(Outcome::Pasted);
        drop(timer);
        let records = captured_for(op_id);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert!(record.cold, "model was not resident at recording start");
        assert_eq!(record.mode, Mode::Cleanup);
        assert_eq!(record.language, "pl");
        assert_eq!(record.model_loads_delta, 1);
        assert_eq!(record.worker_spawns_delta, 1);
        assert_eq!(record.worker_pid_before, before.worker_pid);
        assert_eq!(record.worker_pid_after, 4321);
        // The snapshot is consumed: a stop without a start counts as warm.
        let lone = OperationTimer::begin_stop(Mode::Raw, String::new());
        let lone_id = op_id_of(&lone);
        assert_ne!(lone_id, op_id);
        lone.finish(Outcome::Pasted);
        drop(lone);
        assert!(!captured_for(lone_id)[0].cold);
    }

    #[test]
    fn active_operation_slot_is_released_on_drop() {
        let _serial = lock(&GLOBAL_STATE);
        let before = ACTIVE_OPS.load(Ordering::Relaxed);
        let first = OperationTimer::begin_stop(Mode::Raw, "en".into());
        let second = OperationTimer::begin_stop(Mode::Raw, "en".into());
        assert_eq!(ACTIVE_OPS.load(Ordering::Relaxed), before + 2);
        assert_eq!(second.inner().active_ops, before + 2);
        let first_id = op_id_of(&first);
        let second_id = op_id_of(&second);
        drop(first);
        assert_eq!(ACTIVE_OPS.load(Ordering::Relaxed), before + 1);
        drop(second);
        assert_eq!(ACTIVE_OPS.load(Ordering::Relaxed), before);
        // Overlap is visible in the second operation's record.
        assert!(
            violations(&captured_for(second_id)[0]).contains(&Invariant::MultipleActiveOperations)
        );
        assert!(
            !violations(&captured_for(first_id)[0]).contains(&Invariant::MultipleActiveOperations)
        );
    }

    #[test]
    fn record_is_emitted_once_when_the_last_clone_drops() {
        let _serial = lock(&GLOBAL_STATE);
        let timer = OperationTimer::begin_stop(Mode::Raw, "en".into());
        let op_id = op_id_of(&timer);
        let clone = timer.clone();
        drop(timer);
        assert!(captured_for(op_id).is_empty(), "a clone is still alive");
        clone.finish(Outcome::Pasted);
        assert!(captured_for(op_id).is_empty(), "finish alone must not emit");
        drop(clone);
        let records = captured_for(op_id);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, Outcome::Pasted);
    }

    #[test]
    fn early_return_paths_report_their_outcome_and_unfinished_ones_abort() {
        let _serial = lock(&GLOBAL_STATE);
        let early = OperationTimer::begin_stop(Mode::Raw, "en".into());
        let early_id = op_id_of(&early);
        early.set_audio_samples(0);
        early.finish(Outcome::EmptyAudio);
        // A later path must not overwrite the first outcome.
        early.finish(Outcome::Pasted);
        drop(early);
        assert_eq!(captured_for(early_id)[0].outcome, Outcome::EmptyAudio);

        let forgotten = OperationTimer::begin_stop(Mode::Raw, "en".into());
        let forgotten_id = op_id_of(&forgotten);
        drop(forgotten);
        assert_eq!(captured_for(forgotten_id)[0].outcome, Outcome::Aborted);
    }

    #[test]
    fn audio_duration_is_derived_from_16khz_samples() {
        let _serial = lock(&GLOBAL_STATE);
        let timer = OperationTimer::begin_stop(Mode::Raw, "en".into());
        timer.set_audio_samples(80_000);
        assert_eq!(timer.inner().audio_ms, Some(5_000));
        timer.set_audio_samples(8_000);
        assert_eq!(timer.inner().audio_ms, Some(500));
        timer.finish(Outcome::Pasted);
    }

    #[test]
    fn labels_are_reduced_to_a_safe_vocabulary() {
        assert_eq!(sanitize_label("pl"), Some("pl".to_string()));
        assert_eq!(
            sanitize_label("en-US_5s.v2"),
            Some("en-US_5s.v2".to_string())
        );
        assert_eq!(
            sanitize_label("my text!/\\ here"),
            Some("mytexthere".to_string())
        );
        assert_eq!(sanitize_label("   "), None);
        assert_eq!(
            sanitize_label(&"a".repeat(100)).unwrap().len(),
            MAX_LABEL_LEN
        );
    }

    #[test]
    fn first_outcome_wins_and_unfinished_operations_are_aborted() {
        let mut op = inner(Mode::Raw, true);
        op.finish(Outcome::PasteError, 10, counters_at(2, 1, 100, 5));
        op.finish(Outcome::Pasted, 99, counters_at(9, 9, 9, 9));
        let record = op.build_record(500, Counters::default());
        assert_eq!(record.outcome, Outcome::PasteError);
        assert_eq!(record.total_us, 10);
        assert_eq!(record.model_loads_delta, 0);

        let op = inner(Mode::Raw, true);
        let record = op.build_record(500, counters_at(2, 1, 100, 5));
        assert_eq!(record.outcome, Outcome::Aborted);
        assert_eq!(record.total_us, 500);
    }

    #[test]
    fn json_line_contains_only_expected_numeric_and_enum_fields() {
        let mut op = inner(Mode::Cleanup, false);
        op.audio_ms = Some(5_000);
        op.stt_path = Some(SttPath::StreamFinalize);
        op.stages.stt_us = Some(1);
        op.engine_pending_at_stt = Some(0);
        let record = finished_record(&mut op, Outcome::Pasted, 10, counters_at(3, 2, 222, 6));
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        let object = value.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "active_ops",
                "app_version",
                "audio_ms",
                "build",
                "cold",
                "dropped_records",
                "engine_pending_at_stt",
                "language",
                "llm_requests",
                "mode",
                "model_loads_delta",
                "op_id",
                "outcome",
                "paste_calls",
                "schema",
                "seq",
                "session",
                "stages",
                "stt_path",
                "total_us",
                "worker_pid_after",
                "worker_pid_before",
                "worker_spawns_delta"
            ]
        );
        // Every string field is a closed vocabulary or a setting, never free text.
        assert_eq!(object["mode"], "cleanup");
        assert_eq!(object["outcome"], "pasted");
        assert_eq!(object["stt_path"], "stream_finalize");
        assert_eq!(object["language"], "en");
        assert_eq!(object["stages"], serde_json::json!({ "stt_us": 1 }));
        assert_eq!(object["audio_ms"], 5_000);
    }

    #[test]
    fn full_sink_drops_with_a_count_and_never_blocks() {
        let (sink, rx) = Sink::with_capacity(2);
        let mut op = inner(Mode::Raw, true);
        let record = finished_record(&mut op, Outcome::Pasted, 10, counters_at(2, 1, 100, 5));
        for _ in 0..5 {
            sink.enqueue(record.clone());
        }
        assert_eq!(sink.dropped(), 3);
        assert_eq!(rx.try_iter().count(), 2);
        // Space again: the next record is accepted.
        sink.enqueue(record);
        assert_eq!(sink.dropped(), 3);
        assert_eq!(rx.try_iter().count(), 1);
    }

    #[test]
    fn closed_writer_counts_drops_instead_of_panicking() {
        let (sink, rx) = Sink::with_capacity(2);
        drop(rx);
        let mut op = inner(Mode::Raw, true);
        let record = finished_record(&mut op, Outcome::Pasted, 10, counters_at(2, 1, 100, 5));
        sink.enqueue(record);
        assert_eq!(sink.dropped(), 1);
    }

    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writer_reports_the_first_failure_once_and_counts_the_rest() {
        let (tx, rx) = mpsc::sync_channel(8);
        let mut op = inner(Mode::Raw, true);
        let record = finished_record(&mut op, Outcome::Pasted, 10, counters_at(2, 1, 100, 5));
        for _ in 0..3 {
            tx.send(Message::Record(record.clone())).unwrap();
        }
        drop(tx);
        let mut reports = Vec::new();
        let (written, failed) = run_writer(rx, FailingWriter, |m| reports.push(m.to_string()));
        assert_eq!((written, failed), (0, 3));
        assert_eq!(reports, vec!["disk full".to_string()]);
    }

    #[test]
    fn writer_emits_one_parseable_json_line_per_record() {
        let (tx, rx) = mpsc::sync_channel(8);
        let mut op = inner(Mode::Raw, true);
        let record = finished_record(&mut op, Outcome::Pasted, 10, counters_at(2, 1, 100, 5));
        tx.send(Message::Record(record.clone())).unwrap();
        tx.send(Message::Record(record)).unwrap();
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let end = SessionEnd {
            kind: "session_end",
            schema: SCHEMA_VERSION,
            session: 9,
            records: 2,
            dropped_records: 0,
        };
        tx.send(Message::End(end, done_tx)).unwrap();
        drop(tx);
        let mut buffer = Vec::new();
        let (written, failed) = run_writer(rx, &mut buffer, |_| panic!("no error expected"));
        assert_eq!((written, failed), (3, 0));
        assert!(done_rx.try_recv().is_ok(), "the end marker is acknowledged");
        let text = String::from_utf8(buffer).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines[..2] {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["schema"], SCHEMA_VERSION);
            assert_eq!(value["outcome"], "pasted");
        }
        let end: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(
            end,
            serde_json::json!({
                "kind": "session_end", "schema": 1, "session": 9,
                "records": 2, "dropped_records": 0
            })
        );
    }

    #[test]
    fn pending_guard_counts_while_alive() {
        // A private counter: concurrent engine commands cannot disturb it.
        let counter = AtomicU32::new(0);
        let first = PendingGuard::enter(&counter);
        let second = PendingGuard::enter(&counter);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        drop(first);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        drop(second);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn mode_follows_the_post_process_flag() {
        assert_eq!(Mode::from_post_process(false), Mode::Raw);
        assert_eq!(Mode::from_post_process(true), Mode::Cleanup);
    }
}
