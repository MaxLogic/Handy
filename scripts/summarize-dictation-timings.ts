#!/usr/bin/env bun
/**
 * Summarizes dictation timing JSONL written by the app (see
 * docs/dictation-timing.md) into per-workload statistics.
 *
 *   bun scripts/summarize-dictation-timings.ts --input <file> --samples 30
 *       [--baseline <file>] [--warmup 0] [--cold 0] [--durations 5,15,60]
 *       [--modes raw,cleanup] [--languages en,pl] [--allow-synthetic]
 *
 * Defaults follow the protocol: --warmup 3, --cold 5, --languages en,pl,
 * --durations 5,15,60, --modes raw,cleanup. Pass explicit values to narrow it.
 *
 * A workload is language x mode x duration bucket. Percentiles use the
 * nearest-rank method (rank = ceil(p/100 * n), 1-based; the median is the
 * 50th nearest-rank percentile, so for an even count it is the lower middle
 * value). Statistics are over `total_us`, the accepted stop event until the
 * output completed or was refused.
 *
 * Exit codes: 0 ok, 1 problem with the data (invalid or missing samples,
 * lost records, invariant violations, regression against --baseline),
 * 2 usage error.
 */
import { readFileSync } from "node:fs";

export const SCHEMA_VERSION = 1;
export const MAX_ENGINE_PENDING = 2;
export const STAGE_SUM_TOLERANCE_US = 1_000;
export const REGRESSION_FRACTION = 0.1;
export const REGRESSION_FLOOR_US = 50_000;
/** Protocol defaults; every one can be overridden on the command line. */
export const DEFAULT_WARMUP = 3;
export const DEFAULT_COLD = 5;
export const DEFAULT_LANGUAGES = ["en", "pl"];

const OUTCOMES = [
  "pasted",
  "empty_output",
  "paste_error",
  "main_thread_error",
  "transcription_error",
  "cancelled",
  "no_samples",
  "empty_audio",
  "aborted",
] as const;
export type Outcome = (typeof OUTCOMES)[number];

const STAGE_KEYS = [
  "stop_sync_us",
  "task_sched_us",
  "audio_finalize_us",
  "stt_us",
  "wav_wait_us",
  "text_cleanup_us",
  "llm_us",
  "history_us",
  "main_sched_us",
  "paste_us",
] as const;
export type StageKey = (typeof STAGE_KEYS)[number];

export interface TimingRecord {
  schema: number;
  session: number;
  seq: number;
  op_id: number;
  mode: "raw" | "cleanup";
  outcome: Outcome;
  language: string;
  label?: string;
  cold: boolean;
  audio_ms?: number;
  stages: Partial<Record<StageKey, number>>;
  total_us: number;
  model_loads_delta: number;
  worker_spawns_delta: number;
  worker_pid_before: number;
  worker_pid_after: number;
  llm_requests: number;
  paste_calls: number;
  active_ops: number;
  engine_pending_at_stt?: number;
  dropped_records: number;
  synthetic?: boolean;
}

export type InvariantName =
  | "multiple_active_operations"
  | "reload_during_warm_utterance"
  | "llm_request_in_raw_mode"
  | "duplicate_paste"
  | "engine_pending_exceeded"
  | "stage_sum_exceeds_total";

/** Mirrors `violations` in src-tauri/src/dictation_timing.rs. */
export function violations(r: TimingRecord): InvariantName[] {
  const found: InvariantName[] = [];
  if (r.active_ops > 1) found.push("multiple_active_operations");
  if (!r.cold && (r.model_loads_delta > 0 || r.worker_spawns_delta > 0)) {
    found.push("reload_during_warm_utterance");
  }
  if (r.mode === "raw" && r.llm_requests > 0) {
    found.push("llm_request_in_raw_mode");
  }
  if (r.paste_calls > 1) found.push("duplicate_paste");
  if (
    r.engine_pending_at_stt !== undefined &&
    r.engine_pending_at_stt > MAX_ENGINE_PENDING
  ) {
    found.push("engine_pending_exceeded");
  }
  if (stageSum(r) > r.total_us + STAGE_SUM_TOLERANCE_US) {
    found.push("stage_sum_exceeds_total");
  }
  return found;
}

function stageSum(r: TimingRecord): number {
  let sum = 0;
  for (const key of STAGE_KEYS) sum += r.stages[key] ?? 0;
  return sum;
}

// ---------------------------------------------------------------------------
// Parsing and validation
// ---------------------------------------------------------------------------

/** Written by the app when it exits cleanly. */
export interface SessionEnd {
  kind: "session_end";
  schema: number;
  session: number;
  records: number;
  dropped_records: number;
}

export interface ParseResult {
  records: TimingRecord[];
  ends: SessionEnd[];
  errors: string[];
}

export function validateSessionEnd(o: Record<string, unknown>): string | null {
  if (o.schema !== SCHEMA_VERSION) return "unsupported schema";
  for (const key of ["session", "records", "dropped_records"]) {
    if (!isCount(o[key])) return `${key} must be a non-negative integer`;
  }
  return null;
}

function isCount(v: unknown): v is number {
  return typeof v === "number" && Number.isSafeInteger(v) && v >= 0;
}

/** Validates one parsed JSON value; returns the problem or null. */
export function validateRecord(v: unknown): string | null {
  if (typeof v !== "object" || v === null || Array.isArray(v)) {
    return "not a JSON object";
  }
  const o = v as Record<string, unknown>;
  if (o.schema !== SCHEMA_VERSION) {
    return `unsupported schema ${JSON.stringify(o.schema)}`;
  }
  for (const key of [
    "session",
    "seq",
    "op_id",
    "total_us",
    "model_loads_delta",
    "worker_spawns_delta",
    "worker_pid_before",
    "worker_pid_after",
    "llm_requests",
    "paste_calls",
    "active_ops",
    "dropped_records",
  ]) {
    if (!isCount(o[key])) return `${key} must be a non-negative integer`;
  }
  if (o.mode !== "raw" && o.mode !== "cleanup")
    return "mode must be raw|cleanup";
  if (!OUTCOMES.includes(o.outcome as Outcome)) return "unknown outcome";
  if (typeof o.language !== "string" || o.language === "") {
    return "language must be a non-empty string";
  }
  if (
    o.label !== undefined &&
    (typeof o.label !== "string" || o.label === "")
  ) {
    return "label must be a non-empty string";
  }
  if (typeof o.cold !== "boolean") return "cold must be a boolean";
  if (o.audio_ms !== undefined && !isCount(o.audio_ms)) {
    return "audio_ms must be a non-negative integer";
  }
  if (
    o.engine_pending_at_stt !== undefined &&
    !isCount(o.engine_pending_at_stt)
  ) {
    return "engine_pending_at_stt must be a non-negative integer";
  }
  const stages = o.stages;
  if (typeof stages !== "object" || stages === null || Array.isArray(stages)) {
    return "stages must be an object";
  }
  for (const [k, val] of Object.entries(stages)) {
    if (!(STAGE_KEYS as readonly string[]).includes(k)) {
      return `unknown stage ${k}`;
    }
    if (!isCount(val)) return `stage ${k} must be a non-negative integer`;
  }
  if (o.outcome === "pasted" && o.audio_ms === undefined) {
    return "a pasted record needs audio_ms";
  }
  return null;
}

export function parseJsonl(text: string): ParseResult {
  const records: TimingRecord[] = [];
  const ends: SessionEnd[] = [];
  const errors: string[] = [];
  const lines = text.split(/\r?\n/);
  lines.forEach((line, index) => {
    if (line.trim() === "") return;
    let value: unknown;
    try {
      value = JSON.parse(line);
    } catch {
      errors.push(`line ${index + 1}: invalid JSON`);
      return;
    }
    if (
      typeof value === "object" &&
      value !== null &&
      (value as Record<string, unknown>).kind === "session_end"
    ) {
      const endProblem = validateSessionEnd(value as Record<string, unknown>);
      if (endProblem !== null) errors.push(`line ${index + 1}: ${endProblem}`);
      else ends.push(value as SessionEnd);
      return;
    }
    const problem = validateRecord(value);
    if (problem !== null) {
      errors.push(`line ${index + 1}: ${problem}`);
      return;
    }
    records.push(value as TimingRecord);
  });
  return { records, ends, errors };
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/** Nearest-rank percentile of an ascending-sorted, non-empty array. */
export function nearestRank(sorted: number[], p: number): number {
  if (sorted.length === 0) throw new Error("no values");
  const rank = Math.max(1, Math.ceil((p / 100) * sorted.length));
  return sorted[Math.min(rank, sorted.length) - 1];
}

export interface Stats {
  n: number;
  median: number;
  p95: number;
  min: number;
  max: number;
}

export function stats(values: number[]): Stats {
  const sorted = [...values].sort((a, b) => a - b);
  return {
    n: sorted.length,
    median: nearestRank(sorted, 50),
    p95: nearestRank(sorted, 95),
    min: sorted[0],
    max: sorted[sorted.length - 1],
  };
}

/**
 * Nominal duration (seconds) whose bucket contains `audioMs`, or null.
 * A bucket spans nominal +/- max(1 s, 30% of nominal).
 */
export function durationBucket(
  audioMs: number,
  durationsSec: number[],
): number | null {
  let best: number | null = null;
  let bestDiff = Infinity;
  for (const d of durationsSec) {
    const diff = Math.abs(audioMs - d * 1000);
    const tolerance = Math.max(1000, 0.3 * d * 1000);
    if (diff <= tolerance && diff < bestDiff) {
      best = d;
      bestDiff = diff;
    }
  }
  return best;
}

export interface Options {
  samples: number;
  warmup: number;
  cold: number;
  durations: number[];
  modes: string[];
  languages: string[];
  allowSynthetic: boolean;
}

export interface WorkloadSummary {
  key: string;
  language: string;
  mode: string;
  durationSec: number;
  /** Warm successes available after dropping warmups. */
  available: number;
  total: Stats | null;
  stages: Partial<Record<StageKey, Stats>>;
  failures: Partial<Record<Outcome, number>>;
}

export interface Summary {
  recordCount: number;
  synthetic: boolean;
  workloads: WorkloadSummary[];
  cold: { count: number; totalsUs: number[] };
  unbucketed: number;
  invariants: Partial<Record<InvariantName, number>>;
  problems: string[];
  notes: string[];
}

function orderKey(a: TimingRecord, b: TimingRecord): number {
  return a.session - b.session || a.seq - b.seq;
}

/**
 * Detects duplicated, missing or dropped records inside each session. A
 * session's `seq` starts at 1 and its end marker states how many records were
 * assigned a `seq`, so a lost first, middle or last record is all detectable.
 * A session without an end marker (crash, or the app was still running) is
 * reported too.
 */
export function sequenceProblems(
  records: TimingRecord[],
  ends: SessionEnd[],
): string[] {
  const problems: string[] = [];
  const bySession = new Map<number, TimingRecord[]>();
  for (const r of records) {
    const list = bySession.get(r.session) ?? [];
    list.push(r);
    bySession.set(r.session, list);
  }
  for (const e of ends)
    if (!bySession.has(e.session)) bySession.set(e.session, []);
  for (const [session, list] of bySession) {
    list.sort(orderKey);
    let expected = 1;
    for (const r of list) {
      if (r.seq < expected) {
        problems.push(`session ${session}: duplicate seq ${r.seq}`);
      } else if (r.seq > expected) {
        problems.push(
          `session ${session}: records ${expected}..${r.seq - 1} are missing`,
        );
        expected = r.seq + 1;
      } else {
        expected = r.seq + 1;
      }
    }
    const lastSeq = expected - 1;
    const sessionEnds = ends.filter((e) => e.session === session);
    if (sessionEnds.length === 0) {
      problems.push(
        `session ${session}: no end marker (app crashed or was still running)`,
      );
    } else {
      const end = sessionEnds[sessionEnds.length - 1];
      if (end.records > lastSeq) {
        problems.push(
          `session ${session}: records ${lastSeq + 1}..${end.records} are missing (end marker says ${end.records})`,
        );
      } else if (end.records < lastSeq) {
        problems.push(
          `session ${session}: end marker says ${end.records} record(s) but seq reaches ${lastSeq}`,
        );
      }
    }
    const dropped = Math.max(
      0,
      ...list.map((r) => r.dropped_records),
      ...sessionEnds.map((e) => e.dropped_records),
    );
    if (dropped > 0) {
      problems.push(`session ${session}: ${dropped} record(s) were dropped`);
    }
  }
  return problems;
}

/** The workload's language: the run label when set, else the setting. */
export function workloadLanguage(r: TimingRecord): string {
  return r.label ?? r.language;
}

export function summarize(
  records: TimingRecord[],
  opts: Options,
  ends: SessionEnd[] = [],
): Summary {
  const problems: string[] = [];
  const notes: string[] = [];
  problems.push(...sequenceProblems(records, ends));

  if (records.some((r) => r.synthetic === true) && !opts.allowSynthetic) {
    problems.push(
      "synthetic records are not measurements (pass --allow-synthetic to summarize test data)",
    );
  }
  const ambiguous = records.filter(
    (r) => r.label === undefined && r.language === "auto",
  ).length;
  if (ambiguous > 0) {
    problems.push(
      `${ambiguous} record(s) have language "auto" and no label, so spoken languages cannot be told apart; set HANDY_DICTATION_TIMING_LABEL`,
    );
  }

  const invariants: Partial<Record<InvariantName, number>> = {};
  for (const r of records) {
    for (const name of violations(r)) {
      invariants[name] = (invariants[name] ?? 0) + 1;
    }
  }
  const violationTotal = Object.values(invariants).reduce((a, b) => a + b, 0);
  if (violationTotal > 0) {
    problems.push(`${violationTotal} invariant violation(s)`);
  }

  const seenLanguages = [...new Set(records.map(workloadLanguage))].sort();
  const languages = opts.languages;
  if (languages.length === 0) problems.push("no records");
  for (const seen of seenLanguages) {
    if (!languages.includes(seen)) {
      problems.push(`unexpected workload language/label "${seen}"`);
    }
  }

  const workloads: WorkloadSummary[] = [];
  let unbucketed = 0;
  const cold: number[] = [];
  const warmByKey = new Map<string, TimingRecord[]>();
  const failuresByKey = new Map<string, Partial<Record<Outcome, number>>>();
  const keyOf = (language: string, mode: string, duration: number) =>
    `${language}|${mode}|${duration}`;

  for (const r of [...records].sort(orderKey)) {
    const bucket =
      r.audio_ms === undefined
        ? null
        : durationBucket(r.audio_ms, opts.durations);
    if (r.outcome === "pasted" && r.cold) {
      cold.push(r.total_us);
      continue;
    }
    if (bucket === null) {
      unbucketed += 1;
      continue;
    }
    const key = keyOf(workloadLanguage(r), r.mode, bucket);
    if (r.outcome !== "pasted") {
      const counts = failuresByKey.get(key) ?? {};
      counts[r.outcome] = (counts[r.outcome] ?? 0) + 1;
      failuresByKey.set(key, counts);
      continue;
    }
    const list = warmByKey.get(key) ?? [];
    list.push(r);
    warmByKey.set(key, list);
  }

  for (const language of languages) {
    for (const mode of opts.modes) {
      for (const durationSec of opts.durations) {
        const key = keyOf(language, mode, durationSec);
        const all = warmByKey.get(key) ?? [];
        const usable = all.slice(opts.warmup);
        const taken = usable.slice(0, opts.samples);
        const label = `${language} ${mode} ${durationSec}s`;
        if (taken.length < opts.samples) {
          problems.push(
            `${label}: ${taken.length} warm sample(s) after ${opts.warmup} warmup, need ${opts.samples}`,
          );
        } else if (usable.length > opts.samples) {
          notes.push(
            `${label}: ${usable.length - opts.samples} extra sample(s) after the first ${opts.samples} were ignored`,
          );
        }
        const stageStats: Partial<Record<StageKey, Stats>> = {};
        for (const stage of STAGE_KEYS) {
          const values = taken
            .map((r) => r.stages[stage])
            .filter((v): v is number => v !== undefined);
          if (values.length > 0) stageStats[stage] = stats(values);
        }
        workloads.push({
          key,
          language,
          mode,
          durationSec,
          available: usable.length,
          total: taken.length > 0 ? stats(taken.map((r) => r.total_us)) : null,
          stages: stageStats,
          failures: failuresByKey.get(key) ?? {},
        });
      }
    }
  }

  if (cold.length < opts.cold) {
    problems.push(`${cold.length} cold success(es), need ${opts.cold}`);
  }

  return {
    recordCount: records.length,
    synthetic: records.some((r) => r.synthetic === true),
    workloads,
    cold: { count: cold.length, totalsUs: cold },
    unbucketed,
    invariants,
    problems,
    notes,
  };
}

// ---------------------------------------------------------------------------
// Baseline comparison
// ---------------------------------------------------------------------------

export interface Comparison {
  key: string;
  label: string;
  baselineP95: number;
  candidateP95: number;
  delta: number;
  threshold: number;
  regressed: boolean;
}

/** Candidate p95 may exceed the baseline by max(10% of baseline p95, 50 ms). */
export function regressionThreshold(baselineP95Us: number): number {
  return Math.max(REGRESSION_FRACTION * baselineP95Us, REGRESSION_FLOOR_US);
}

export function compare(
  candidate: Summary,
  baseline: Summary,
): { rows: Comparison[]; problems: string[] } {
  const rows: Comparison[] = [];
  const problems: string[] = [];
  const baselineByKey = new Map(baseline.workloads.map((w) => [w.key, w]));
  for (const w of candidate.workloads) {
    const label = `${w.language} ${w.mode} ${w.durationSec}s`;
    const base = baselineByKey.get(w.key);
    if (!base?.total || !w.total) {
      problems.push(
        `${label}: no samples in ${base?.total ? "candidate" : "baseline"} to compare`,
      );
      continue;
    }
    const threshold = regressionThreshold(base.total.p95);
    const delta = w.total.p95 - base.total.p95;
    rows.push({
      key: w.key,
      label,
      baselineP95: base.total.p95,
      candidateP95: w.total.p95,
      delta,
      threshold,
      regressed: delta > threshold,
    });
  }
  return { rows, problems };
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

const ms = (us: number) => (us / 1000).toFixed(1);

export function formatSummary(
  s: Summary,
  opts: Options,
  title: string,
): string {
  const out: string[] = [];
  out.push(`== ${title}: ${s.recordCount} record(s) ==`);
  if (s.synthetic) {
    out.push("*** SYNTHETIC DATA: not a measurement of the real app ***");
  }
  out.push(
    `samples per workload: ${opts.samples} (warmup dropped: ${opts.warmup}); nearest-rank percentiles over total_us, ms`,
  );
  for (const w of s.workloads) {
    const head = `${w.language} ${w.mode} ${w.durationSec}s`;
    if (!w.total) {
      out.push(`${head}: MISSING (0 warm samples)`);
    } else {
      const t = w.total;
      out.push(
        `${head}: n=${t.n} median=${ms(t.median)} p95=${ms(t.p95)} min=${ms(t.min)} max=${ms(t.max)}` +
          (t.n < opts.samples ? `  SHORT of ${opts.samples}` : ""),
      );
      const parts = STAGE_KEYS.filter((k) => w.stages[k]).map((k) => {
        const st = w.stages[k] as Stats;
        return `${k.replace(/_us$/, "")} ${ms(st.median)}/${ms(st.p95)}`;
      });
      if (parts.length > 0)
        out.push(`    stage median/p95: ${parts.join(", ")}`);
    }
    const failures = Object.entries(w.failures);
    if (failures.length > 0) {
      out.push(
        `    failures: ${failures.map(([k, v]) => `${k}=${v}`).join(", ")}`,
      );
    }
  }
  if (s.cold.count > 0) {
    const st = stats(s.cold.totalsUs);
    out.push(
      `cold starts: n=${st.n} median=${ms(st.median)} p95=${ms(st.p95)} min=${ms(st.min)} max=${ms(st.max)} (listed: ${s.cold.totalsUs.map(ms).join(", ")})`,
    );
  } else {
    out.push("cold starts: none");
  }
  if (s.unbucketed > 0) {
    out.push(`records outside every duration bucket: ${s.unbucketed}`);
  }
  const inv = Object.entries(s.invariants);
  out.push(
    inv.length === 0
      ? "invariant violations: 0"
      : `invariant violations: ${inv.map(([k, v]) => `${k}=${v}`).join(", ")}`,
  );
  for (const n of s.notes) out.push(`note: ${n}`);
  for (const p of s.problems) out.push(`PROBLEM: ${p}`);
  return out.join("\n");
}

export function formatComparison(rows: Comparison[]): string {
  const out = ["== regression vs baseline (p95 of total, ms) =="];
  out.push(
    `allowed increase: max(${REGRESSION_FRACTION * 100}% of baseline p95, ${REGRESSION_FLOOR_US / 1000} ms)`,
  );
  for (const r of rows) {
    out.push(
      `${r.label}: baseline=${ms(r.baselineP95)} candidate=${ms(r.candidateP95)} delta=${r.delta >= 0 ? "+" : ""}${ms(r.delta)} allowed=${ms(r.threshold)} ${r.regressed ? "REGRESSION" : "ok"}`,
    );
  }
  return out.join("\n");
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

interface Args {
  input: string;
  baseline?: string;
  opts: Options;
}

class UsageError extends Error {}

function intArg(name: string, raw: string | undefined, min: number): number {
  const n = Number(raw);
  if (raw === undefined || !Number.isInteger(n) || n < min) {
    throw new UsageError(`${name} needs an integer >= ${min}`);
  }
  return n;
}

function listArg(name: string, raw: string | undefined): string[] {
  if (raw === undefined || raw.trim() === "") {
    throw new UsageError(`${name} needs a comma-separated list`);
  }
  return raw
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s !== "");
}

export function parseArgs(argv: string[]): Args {
  const values = new Map<string, string>();
  let allowSynthetic = false;
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    if (!flag.startsWith("--"))
      throw new UsageError(`unexpected argument ${flag}`);
    const name = flag.slice(2);
    if (name === "allow-synthetic") {
      allowSynthetic = true;
      continue;
    }
    if (
      ![
        "input",
        "samples",
        "baseline",
        "warmup",
        "cold",
        "durations",
        "modes",
        "languages",
      ].includes(name)
    ) {
      throw new UsageError(`unknown option ${flag}`);
    }
    i += 1;
    if (i >= argv.length) throw new UsageError(`${flag} needs a value`);
    values.set(name, argv[i]);
  }
  const input = values.get("input");
  if (input === undefined) throw new UsageError("--input is required");
  if (!values.has("samples")) throw new UsageError("--samples is required");
  const durations = values.has("durations")
    ? listArg("--durations", values.get("durations")).map((d) => {
        const n = Number(d);
        if (!(n > 0))
          throw new UsageError("--durations needs positive numbers");
        return n;
      })
    : [5, 15, 60];
  const modes = values.has("modes")
    ? listArg("--modes", values.get("modes"))
    : ["raw", "cleanup"];
  for (const m of modes) {
    if (m !== "raw" && m !== "cleanup")
      throw new UsageError(`unknown mode ${m}`);
  }
  return {
    input,
    baseline: values.get("baseline"),
    opts: {
      samples: intArg("--samples", values.get("samples"), 1),
      warmup: values.has("warmup")
        ? intArg("--warmup", values.get("warmup"), 0)
        : DEFAULT_WARMUP,
      cold: values.has("cold")
        ? intArg("--cold", values.get("cold"), 0)
        : DEFAULT_COLD,
      durations,
      modes,
      languages: values.has("languages")
        ? listArg("--languages", values.get("languages"))
        : DEFAULT_LANGUAGES,
      allowSynthetic,
    },
  };
}

export function run(argv: string[], log: (line: string) => void): number {
  let args: Args;
  try {
    args = parseArgs(argv);
  } catch (e) {
    if (e instanceof UsageError) {
      log(`error: ${e.message}`);
      log(
        "usage: bun scripts/summarize-dictation-timings.ts --input <file> --samples <n> [--baseline <file>] [--warmup n] [--cold n] [--durations 5,15,60] [--modes raw,cleanup] [--languages en,pl] [--allow-synthetic]",
      );
      return 2;
    }
    throw e;
  }

  let failed = false;
  const summarizeFile = (path: string, title: string): Summary | null => {
    let text: string;
    try {
      text = readFileSync(path, "utf8");
    } catch (e) {
      log(`PROBLEM: cannot read ${path}: ${(e as Error).message}`);
      failed = true;
      return null;
    }
    const { records, ends, errors } = parseJsonl(text);
    for (const err of errors) log(`PROBLEM: ${path}: ${err}`);
    if (errors.length > 0) failed = true;
    if (records.length === 0) {
      log(`PROBLEM: ${path}: no valid records`);
      failed = true;
      return null;
    }
    const summary = summarize(records, args.opts, ends);
    log(formatSummary(summary, args.opts, title));
    if (summary.problems.length > 0) failed = true;
    return summary;
  };

  const candidate = summarizeFile(args.input, "candidate");
  if (args.baseline !== undefined) {
    const baseline = summarizeFile(args.baseline, "baseline");
    if (candidate && baseline) {
      const { rows, problems } = compare(candidate, baseline);
      log(formatComparison(rows));
      for (const p of problems) log(`PROBLEM: ${p}`);
      if (problems.length > 0 || rows.some((r) => r.regressed)) failed = true;
    }
  }
  return failed ? 1 : 0;
}

if (import.meta.main) {
  process.exit(run(process.argv.slice(2), (line) => console.log(line)));
}
