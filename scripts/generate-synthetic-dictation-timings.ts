#!/usr/bin/env bun
/**
 * Writes deterministic SYNTHETIC dictation timing records in the same JSONL
 * layout the app produces. Used only to prove scripts/summarize-dictation-timings.ts;
 * every record carries `"synthetic": true` and the numbers are invented, so
 * never treat the output as a measurement.
 *
 *   bun scripts/generate-synthetic-dictation-timings.ts --output <file>
 *       [--samples 30] [--warmup 3] [--cold 5] [--languages en] [--scale 1]
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

export interface GenerateOptions {
  samples: number;
  warmup: number;
  cold: number;
  languages: string[];
  durations: number[];
  /** Multiplies every stage; use >1 to fake a slower candidate. */
  scale: number;
}

export const DEFAULTS: GenerateOptions = {
  samples: 30,
  warmup: 3,
  cold: 5,
  languages: ["en"],
  durations: [5, 15, 60],
  scale: 1,
};

function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const SESSION_BASE = 1_700_000_000_000;

export function generateSynthetic(
  options: Partial<GenerateOptions> = {},
): string {
  const defined = Object.fromEntries(
    Object.entries(options).filter(([, v]) => v !== undefined),
  );
  const o: GenerateOptions = { ...DEFAULTS, ...defined };
  const rand = mulberry32(42);
  const jitter = (base: number) =>
    Math.round(base * o.scale * (0.9 + rand() * 0.2));
  const lines: string[] = [];
  let opId = 1;

  const record = (
    session: number,
    seq: number,
    language: string,
    mode: "raw" | "cleanup",
    durationSec: number,
    cold: boolean,
  ) => {
    const stages: Record<string, number> = {
      stop_sync_us: jitter(300),
      task_sched_us: jitter(150),
      audio_finalize_us: jitter(2_000),
      stt_us: jitter(150_000 + durationSec * 12_000 + (cold ? 3_000_000 : 0)),
      wav_wait_us: jitter(400),
      text_cleanup_us: jitter(1_500),
      llm_us:
        mode === "cleanup" ? jitter(500_000 + durationSec * 6_000) : jitter(30),
      history_us: jitter(4_000),
      main_sched_us: jitter(800),
      paste_us: jitter(45_000),
    };
    const sum = Object.values(stages).reduce((a, b) => a + b, 0);
    return JSON.stringify({
      schema: 1,
      session,
      seq,
      op_id: opId++,
      mode,
      outcome: "pasted",
      language,
      label: language,
      build: "release",
      app_version: "0.0.0-synthetic",
      cold,
      audio_ms: Math.round(durationSec * 1000 * (0.97 + rand() * 0.06)),
      stt_path: "batch",
      stages,
      total_us: sum + jitter(3_000),
      model_loads_delta: cold ? 1 : 0,
      worker_spawns_delta: cold ? 1 : 0,
      worker_pid_before: cold ? 0 : 4242,
      worker_pid_after: 4242,
      llm_requests: mode === "cleanup" ? 1 : 0,
      paste_calls: 1,
      active_ops: 1,
      engine_pending_at_stt: 0,
      dropped_records: 0,
      synthetic: true,
    });
  };

  const end = (session: number, records: number) =>
    JSON.stringify({
      kind: "session_end",
      schema: 1,
      session,
      records,
      dropped_records: 0,
    });

  // Each cold start is a fresh app process, hence its own session.
  for (let i = 0; i < o.cold; i++) {
    const mode = i % 2 === 0 ? "raw" : "cleanup";
    lines.push(record(SESSION_BASE + i, 1, o.languages[0], mode, 5, true));
    lines.push(end(SESSION_BASE + i, 1));
  }
  // One app run per language label, since the label is fixed at launch.
  o.languages.forEach((language, index) => {
    const session = SESSION_BASE + 1000 + index;
    let seq = 1;
    for (const mode of ["raw", "cleanup"] as const) {
      for (const duration of o.durations) {
        for (let i = 0; i < o.warmup + o.samples; i++) {
          lines.push(record(session, seq++, language, mode, duration, false));
        }
      }
    }
    lines.push(end(session, seq - 1));
  });
  return lines.join("\n") + "\n";
}

if (import.meta.main) {
  const argv = process.argv.slice(2);
  const values = new Map<string, string>();
  for (let i = 0; i < argv.length; i += 2)
    values.set(argv[i].replace(/^--/, ""), argv[i + 1]);
  const output = values.get("output");
  if (!output) {
    console.error(
      "usage: bun scripts/generate-synthetic-dictation-timings.ts --output <file> [--samples n] [--warmup n] [--cold n] [--languages en,pl] [--scale x]",
    );
    process.exit(2);
  }
  const text = generateSynthetic({
    samples: values.has("samples") ? Number(values.get("samples")) : undefined,
    warmup: values.has("warmup") ? Number(values.get("warmup")) : undefined,
    cold: values.has("cold") ? Number(values.get("cold")) : undefined,
    languages: values.get("languages")?.split(","),
    scale: values.has("scale") ? Number(values.get("scale")) : undefined,
  });
  mkdirSync(dirname(output), { recursive: true });
  writeFileSync(output, text);
  console.log(
    `wrote ${text.trim().split("\n").length} synthetic record(s) to ${output}`,
  );
}
