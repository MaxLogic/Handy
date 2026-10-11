# Dictation timing measurement

How to capture and summarize production timings for the stop-to-output
dictation pipeline (raw and LLM-cleanup routes). Agents cannot run the
real-microphone baseline; follow this procedure yourself.

## What is recorded

With `HANDY_DICTATION_TIMING_FILE` set, the app appends one JSON line per
accepted stop event (implementation: `src-tauri/src/dictation_timing.rs`). The
record holds only numbers, closed-vocabulary enums and the language setting. It
never contains transcript, selection, prompt or credential text.

Recording is off when the variable is unset, so a normal install writes
nothing. File writes happen on a dedicated thread behind a bounded queue of 64
records. If the writer falls behind, records are dropped and counted
(`dropped_records`, plus a gap in `seq`); a write failure is logged once to the
app log. The pipeline never waits for the file.

Key fields (durations are microseconds from `std::time::Instant`):

| Field                   | Meaning                                                                                                                                      |
| ----------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `op_id`                 | Operation ID; the same value appears as `op=` in the app log                                                                                 |
| `session`, `seq`        | Process start (unix ms) and per-process record counter starting at 1; the end marker states the final count, so a lost record is detected    |
| `label`                 | Workload label from `HANDY_DICTATION_TIMING_LABEL` (for example the spoken language)                                                         |
| `mode`                  | `raw` (no LLM) or `cleanup` (existing post-processing route)                                                                                 |
| `outcome`               | `pasted`, `empty_output`, `paste_error`, `main_thread_error`, `transcription_error`, `cancelled`, `no_samples`, `empty_audio`, `aborted`     |
| `cold`                  | Model was not resident when the recording started                                                                                            |
| `audio_ms`              | Captured audio length after the recorder's VAD (not the spoken length), used to bucket 5/15/60 s workloads                                   |
| `total_us`              | Accepted stop event until output completed or was refused                                                                                    |
| `stages`                | `stop_sync`, `task_sched`, `audio_finalize`, `stt`, `wav_wait`, `text_cleanup`, `llm`, `history`, `main_sched`, `paste`                      |
| `model_loads_delta`     | Model load attempts between recording start and completion                                                                                   |
| `worker_spawns_delta`   | Worker processes started in that window; `worker_pid_before/after` show PID changes                                                          |
| `llm_requests`          | LLM requests sent for this operation                                                                                                         |
| `paste_calls`           | Paste invocations for this operation                                                                                                         |
| `active_ops`            | Operations in flight when the stop was accepted, including this one                                                                          |
| `engine_pending_at_stt` | Engine commands outstanding just before STT started (queue depth). Counts transcribe-cpp supervisor commands only; ONNX models always read 0 |

Boundaries to keep in mind when reading the numbers:

- The clock starts when `TranscribeAction::stop` runs, not at the key event.
  Shortcut dispatch before that is not included.
- `stt` includes time spent waiting for the engine queue or a model load that
  was still in progress. Use `cold`, `model_loads_delta` and
  `engine_pending_at_stt` to tell those apart from a warm run.
- `text_cleanup` is the deterministic text post-processing inside the STT
  calls (script conversion, custom words, filler removal, language handling).
  It is timed separately and excluded from `stt`. `llm` is the LLM step only.
- `paste` covers `utils::paste`, including its own clipboard settle and restore
  waits. `main_sched` is the delay before the main thread starts the paste.
- Stages are sequential and disjoint, so their sum must not exceed `total_us`.
- The last record is written after the paste completes. Quit the app from the
  tray (not by killing it) so it writes the `session_end` marker. A session
  without that marker, or whose marker count differs from the records present,
  fails the summary.

### Invariants checked per record

The summarizer fails the run on any of these (the same rules are in
`dictation_timing::violations`):

1. `active_ops` greater than 1 (more than one active operation).
2. A warm record (`cold` false) with `model_loads_delta` or
   `worker_spawns_delta` above 0. This assumes the model is configured to stay
   resident (unload timeout `never`).
3. `llm_requests` above 0 in `raw` mode.
4. `paste_calls` above 1 (duplicate paste).
5. `engine_pending_at_stt` above 2 (unbounded pending work).
6. Stage sum above `total_us` plus 1 ms.

## Manual protocol

Use the exact same conditions for the baseline and every candidate. Do not
change the configuration between them.

1. Build the unmodified revision (baseline) or the candidate as a Windows x64
   Release app. Run it outside a debugger. Set the overlay to `none`.
2. Fix the audio, settings, local STT model, local LLM server and model
   (version and quantization) and the target application. Use the real
   configured LLM endpoint for `cleanup`. Set the model unload timeout to
   `never`. Do not run other GPU workloads.
3. Prepare representative English and Polish speech of about 5, 15 and 60
   seconds, and speak or play it the same way every time (same microphone and
   playback procedure). Also keep short empty, noisy and error cases.
4. Start the app with the output file set, for example in PowerShell:

   ```powershell
   $env:HANDY_DICTATION_TIMING_FILE = "F:\projects\MaxLogic\Handy\artifacts\dictation\baseline.jsonl"
   & "<path to the Release handy.exe>"
   ```

   Use one file per run set (baseline, candidate). The label is fixed when
   the app starts, so restart the app with a different label for each spoken
   language:

   ```powershell
   $env:HANDY_DICTATION_TIMING_LABEL = "en"   # or "pl"
   ```

   The label (`[A-Za-z0-9_.-]`, up to 32 characters) is the workload language in
   the summary. It is required when the language setting is `auto`, because
   English and Polish runs would otherwise merge into one workload.

5. Warm samples, per language, mode and duration: do 3 warmup operations, then
   30 successful operations. Use the "transcribe" binding for `raw` and the
   post-process binding for `cleanup`. Keep the running app between samples so
   the model stays resident.
6. Cold starts: five times, quit the app, start it again, wait for the idle
   tray state without loading the model through any other action, then do one
   operation. Write these to the same file (each start has its own `session`).
7. Failure cases (empty, noisy, error) are recorded the same way. They count as
   failures in the summary and are not part of the required samples.
8. Save the correctness outputs (the pasted text for each run, plus the app log
   with the matching `op=` IDs) separately under `artifacts/dictation/`, for
   example `artifacts/dictation/outputs/`. They hold user text, so they stay in
   the ignored directory and are never mixed into the timing JSONL.
9. Record next to the run, for example in `artifacts/dictation/run.md`:
   hardware, GPU model and driver version, Windows build, `git rev-parse HEAD`
   plus a diff hash (`git diff HEAD | sha256sum`), build configuration, all
   relevant settings, STT model, LLM server version, LLM model and
   quantization, any concurrent GPU workload, and the date.
10. Assistant mode (later slices) uses selections of empty, 100, 2000 and
    20000 characters; do not infer an assistant baseline from raw or cleanup.

## Summarize

```bash
bun scripts/summarize-dictation-timings.ts --input artifacts/dictation/baseline.jsonl --samples 30
```

Options:

- `--input <file>` and `--samples <n>` are required. A workload is language,
  mode and duration bucket (5, 15 and 60 s; an operation falls in a bucket when
  `audio_ms` is within the larger of 1 s or 30% of it). Each required workload needs
  `<n>` successful warm samples after `--warmup <n>` (default 3) are dropped
  per workload in chronological order. Extra samples beyond `<n>` are ignored
  and noted, never selected.
- `--cold <n>` requires at least `<n>` successful cold records (default 5).
- `--languages` lists the required workload languages (labels); the default
  is `en,pl`. A missing one fails the run, and so does a label that is not
  listed. `--modes` and `--durations` default to `raw,cleanup` and `5,15,60`.
  Every default can be overridden to narrow a run, for example
  `--languages en --warmup 0 --cold 0`.
- `--allow-synthetic` is only for the test data below. Synthetic records fail
  the run without it.
- `--baseline <file>` also summarizes the baseline with the same options and
  compares the p95 of `total_us` per workload. A required workload with no
  samples on either side fails the comparison. A candidate regresses when its
  p95 exceeds the baseline p95 by more than `max(10% of baseline p95, 50 ms)`.
  This is a chosen regression guard, not a user-specified absolute target. On a
  noisy result, repeat the run once and investigate before claiming a gain or a
  regression.

Statistics are nearest-rank (rank = ceil(p/100 x n); the median is the 50th
percentile, the lower middle value for an even count) over `total_us`, in ms,
with per-stage median/p95. Failures are counted per workload by outcome.

Exit code 0 means every workload has its samples, every line parsed, `seq` is
gapless from 1 with a matching end marker and no dropped records, no invariant was violated, and (with
`--baseline`) nothing regressed. Exit code 1 reports a data problem and exit
code 2 a usage error.

## Proving the summarizer

`bun run test:dictation-timings` runs the summarizer tests against synthetic
data. `scripts/generate-synthetic-dictation-timings.ts` writes that data;
`scripts/fixtures/dictation-timings/synthetic-baseline.jsonl` is its committed
output. Every synthetic record has `"synthetic": true` and the summary prints a
banner, so it cannot be mistaken for a measurement.
