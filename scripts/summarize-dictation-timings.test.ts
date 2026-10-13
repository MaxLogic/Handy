import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { generateSynthetic } from "./generate-synthetic-dictation-timings";
import {
  durationBucket,
  nearestRank,
  regressionThreshold,
  run,
  stats,
  validateRecord,
  parseJsonl,
  summarize,
  parseArgs,
} from "./summarize-dictation-timings";

function assistantRecord(seq = 1) {
  return {
    schema: 1,
    session: 777,
    seq,
    op_id: seq,
    mode: "assistant",
    outcome: "committed",
    language: "auto",
    cold: false,
    audio_ms: 5000,
    total_us: 1000000,
    capture_us: 9000,
    selection_scalars: 0,
    stages: { llm_us: 100000, commit_us: 100000 },
    model_loads_delta: 0,
    worker_spawns_delta: 0,
    worker_pid_before: 1,
    worker_pid_after: 1,
    llm_requests: 1,
    paste_calls: 1,
    active_ops: 1,
    dropped_records: 0,
    synthetic: true,
  };
}
function assistantText(records = [assistantRecord()]) {
  return (
    records.map((r) => JSON.stringify(r)).join("\n") +
    "\n" +
    JSON.stringify({
      kind: "session_end",
      schema: 1,
      session: 777,
      records: records.length,
      dropped_records: 0,
    }) +
    "\n"
  );
}

describe("assistant records", () => {
  test("accepted and reported separately with capture and commit timing", () => {
    const result = summarizeText(baselineText + assistantText());
    expect(result.code).toBe(0);
    expect(result.output).toContain(
      "assistant (separate from raw/cleanup samples and baseline)",
    );
    expect(result.output).toContain("committed=1");
    expect(result.output).toContain("selection empty");
    expect(result.output).toContain("capture median=9.0");
  });
  test("assistant outcomes are rejected under raw and cleanup", () => {
    for (const mode of ["raw", "cleanup"]) {
      for (const outcome of [
        "committed",
        "commit_refused",
        "commit_failed",
        "commit_uncertain",
        "llm_error",
      ]) {
        expect(validateRecord({ ...assistantRecord(), mode, outcome })).toBe(
          "assistant outcome requires assistant mode",
        );
      }
    }
  });
  test("raw/cleanup delivery outcomes are rejected under assistant", () => {
    for (const outcome of [
      "pasted",
      "cleanup_fallback",
      "paste_error",
      "main_thread_error",
    ]) {
      expect(validateRecord({ ...assistantRecord(), outcome })).toBe(
        "dictation outcome is invalid for assistant mode",
      );
    }
  });
  test("assistant never supplies raw/cleanup samples or cold starts", () => {
    const opts = parseArgs([
      "--input",
      "unused",
      "--samples",
      "1",
      "--warmup",
      "0",
      "--cold",
      "1",
      "--durations",
      "5",
      "--modes",
      "raw",
      "--languages",
      "en",
      "--allow-synthetic",
    ]).opts;
    const record = { ...assistantRecord(), cold: true };
    const parsed = parseJsonl(assistantText([record]));
    expect(parsed.errors).toEqual([]);
    const summary = summarize(parsed.records, opts, parsed.ends);
    expect(summary.workloads[0].available).toBe(0);
    expect(summary.cold.count).toBe(0);
    expect(summary.unbucketed).toBe(0);
    expect(summary.problems).toContain(
      "en raw 5s: 0 warm sample(s) after 0 warmup, need 1",
    );
  });
  test("selection groups and outcome counts preserve refused and failed results", () => {
    const records = [0, 100, 2000, 20000, 20001].map(
      (selection_scalars, i) => ({
        ...assistantRecord(i + 1),
        selection_scalars,
        outcome: i === 0 ? "commit_refused" : "committed",
      }),
    );
    const parsed = parseJsonl(assistantText(records));
    expect(parsed.errors).toEqual([]);
    const opts = parseArgs([
      "--input",
      "unused",
      "--samples",
      "1",
      "--allow-synthetic",
    ]).opts;
    const summary = summarize(parsed.records, opts, parsed.ends);
    expect(summary.assistant.groups.map((g) => g.bucket)).toEqual([
      "empty",
      "<=100",
      "<=2000",
      "<=20000",
      ">20000",
    ]);
    expect(summary.assistant.outcomes.commit_refused).toBe(1);
    expect(summary.assistant.outcomes.committed).toBe(4);
    expect(summary.assistant.groups[0].total).toBeNull();
  });
  test("new optional fields and commit stage remain strictly validated", () => {
    expect(validateRecord({ ...assistantRecord(), capture_us: -1 })).toContain(
      "capture_us",
    );
    expect(
      validateRecord({ ...assistantRecord(), selection_scalars: 0.5 }),
    ).toContain("selection_scalars");
    expect(
      validateRecord({ ...assistantRecord(), stages: { commit_us: -1 } }),
    ).toContain("commit_us");
    const result = summarizeText(
      baselineText + assistantText([{ ...assistantRecord(), paste_calls: 2 }]),
    );
    expect(result.code).toBe(1);
    expect(result.output).toContain("duplicate_paste");
  });
});

const FIXTURE = join(
  import.meta.dir,
  "fixtures",
  "dictation-timings",
  "synthetic-baseline.jsonl",
);

function withTempDir<T>(fn: (dir: string) => T): T {
  const dir = mkdtempSync(join(tmpdir(), "dictation-timings-test-"));
  try {
    return fn(dir);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

function summarizeText(
  text: string,
  extra: string[] = [],
  samples = "30",
): { code: number; output: string } {
  return withTempDir((dir) => {
    const file = join(dir, "in.jsonl");
    writeFileSync(file, text);
    const lines: string[] = [];
    const code = run(
      [
        "--input",
        file,
        "--samples",
        samples,
        "--warmup",
        "3",
        "--allow-synthetic",
        "--languages",
        "en",
        ...extra,
      ],
      (l) => lines.push(l),
    );
    return { code, output: lines.join("\n") };
  });
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type Patch = (r: any) => void;

test("cleanup skips are separate from failures and latency samples", () => {
  const records = baselineText
    .trimEnd()
    .split("\n")
    .map((line) => JSON.parse(line));
  const index = records.findIndex((r) => r.mode === "cleanup" && !r.cold);
  const result = summarizeText(
    mutateLine(baselineText, index, (r) => {
      r.outcome = "cleanup_skipped";
      r.llm_requests = 0;
    }),
  );
  expect(result.code).toBe(1); // A skip cannot replace a required cleanup success sample.
  expect(result.output).toContain("skipped: cleanup_skipped=1");
  expect(result.output).toContain("en cleanup 5s: n=29");
  expect(result.output).not.toContain("failures: cleanup_skipped");
  const record = { ...assistantRecord(), outcome: "cleanup_skipped" };
  expect(validateRecord(record)).toContain("requires cleanup mode");
});

function mutateLine(text: string, index: number, patch: Patch): string {
  const lines = text.trimEnd().split("\n");
  const record = JSON.parse(lines[index]);
  patch(record);
  lines[index] = JSON.stringify(record);
  return lines.join("\n") + "\n";
}

const baselineText = generateSynthetic();
// Each of the 5 cold starts is a record plus its session-end marker.
const FIRST_WARM = 10;

describe("statistics", () => {
  test("nearest-rank percentiles", () => {
    const ten = Array.from({ length: 10 }, (_, i) => i + 1);
    expect(nearestRank(ten, 50)).toBe(5);
    expect(nearestRank(ten, 95)).toBe(10);
    const twenty = Array.from({ length: 20 }, (_, i) => i + 1);
    expect(nearestRank(twenty, 95)).toBe(19);
    expect(nearestRank([7], 95)).toBe(7);
    expect(stats([30, 10, 20])).toEqual({
      n: 3,
      median: 20,
      p95: 30,
      min: 10,
      max: 30,
    });
  });

  test("duration buckets", () => {
    expect(durationBucket(5100, [5, 15, 60])).toBe(5);
    expect(durationBucket(14000, [5, 15, 60])).toBe(15);
    expect(durationBucket(61000, [5, 15, 60])).toBe(60);
    expect(durationBucket(30000, [5, 15, 60])).toBeNull();
  });

  test("regression threshold is max(10%, 50 ms)", () => {
    expect(regressionThreshold(100_000)).toBe(50_000);
    expect(regressionThreshold(1_000_000)).toBe(100_000);
  });
});

describe("summarizer CLI", () => {
  test("cleanup fallback is reported separately and cannot satisfy latency samples", () => {
    const records = baselineText
      .trimEnd()
      .split("\n")
      .map((line) => JSON.parse(line));
    const index = records.findIndex((r) => r.mode === "cleanup" && !r.cold);
    const changed = mutateLine(baselineText, index, (r) => {
      r.outcome = "cleanup_fallback";
      r.total_us = 1;
      r.stages = { llm_us: 1 };
    });
    const { code, output } = summarizeText(changed);
    expect(output).toContain("en cleanup 5s: n=29");
    expect(output).toContain("cleanup_fallback=1");
    expect(output).toContain("need 30");
    expect(output).not.toContain("unknown outcome");
    expect(code).toBe(1);
  });

  test("committed fixture is exactly the generator output", () => {
    expect(readFileSync(FIXTURE, "utf8")).toBe(baselineText);
  });

  test("fixture passes with 30 warm samples per workload and 5 cold", () => {
    const { code, output } = summarizeText(baselineText, ["--cold", "5"]);
    expect(output).toContain("SYNTHETIC DATA");
    expect(output).toContain("en raw 5s: n=30");
    expect(output).toContain("en cleanup 60s: n=30");
    expect(output).toContain("cold starts: n=5");
    expect(output).toContain("invariant violations: 0");
    expect(code).toBe(0);
  });

  test("asking for more samples than exist fails", () => {
    const { code, output } = summarizeText(baselineText, [], "31");
    expect(output).toContain("need 31");
    expect(code).toBe(1);
  });

  test("a missing language workload fails", () => {
    const { code, output } = summarizeText(baselineText, [
      "--languages",
      "en,pl",
    ]);
    expect(output).toContain("pl raw 5s: MISSING");
    expect(code).toBe(1);
  });

  test("too few cold starts fails", () => {
    const { code, output } = summarizeText(baselineText, ["--cold", "6"]);
    expect(output).toContain("5 cold success(es), need 6");
    expect(code).toBe(1);
  });

  test("invalid JSON and invalid fields fail", () => {
    expect(summarizeText(baselineText + "{not json\n").code).toBe(1);
    const bad = mutateLine(baselineText, FIRST_WARM, (r) => {
      r.total_us = -1;
    });
    const result = summarizeText(bad);
    expect(result.output).toContain("total_us must be a non-negative integer");
    expect(result.code).toBe(1);
  });

  test("a missing record is detected from the sequence gap", () => {
    const lines = baselineText.trimEnd().split("\n");
    lines.splice(FIRST_WARM + 10, 1);
    const result = summarizeText(lines.join("\n") + "\n");
    expect(result.output).toContain("are missing");
    expect(result.code).toBe(1);
  });

  test("dropped records reported by the app fail", () => {
    const text = mutateLine(baselineText, FIRST_WARM + 1, (r) => {
      r.dropped_records = 2;
    });
    const result = summarizeText(text);
    expect(result.output).toContain("were dropped");
    expect(result.code).toBe(1);
  });

  const cases: [string, Patch][] = [
    ["llm_request_in_raw_mode", (r) => (r.llm_requests = 1)],
    ["duplicate_paste", (r) => (r.paste_calls = 2)],
    ["multiple_active_operations", (r) => (r.active_ops = 2)],
    ["reload_during_warm_utterance", (r) => (r.worker_spawns_delta = 1)],
    ["engine_pending_exceeded", (r) => (r.engine_pending_at_stt = 3)],
    ["stage_sum_exceeds_total", (r) => (r.total_us = 1000)],
  ];
  test.each(cases)("invariant %s fails the run", (name, patch) => {
    const text = mutateLine(baselineText, FIRST_WARM + 2, patch);
    const result = summarizeText(text);
    expect(result.output).toContain(`${name}=1`);
    expect(result.code).toBe(1);
  });

  test("failures are counted per workload without failing the run", () => {
    // Append a cancelled 5 s raw operation after the last warm record.
    const lines = baselineText.trimEnd().split("\n");
    const endIndex = lines.length - 1;
    const last = JSON.parse(lines[endIndex - 1]);
    const end = JSON.parse(lines[endIndex]);
    const failed = {
      ...last,
      seq: last.seq + 1,
      op_id: last.op_id + 1,
      mode: "raw",
      outcome: "cancelled",
      llm_requests: 0,
      audio_ms: 5000,
    };
    end.records += 1;
    lines.splice(endIndex, 1, JSON.stringify(failed), JSON.stringify(end));
    const result = summarizeText(lines.join("\n") + "\n");
    expect(result.output).toContain("failures: cancelled=1");
    expect(result.code).toBe(0);
  });

  test("synthetic data fails unless explicitly allowed", () => {
    const strict = withTempDir((dir) => {
      const file = join(dir, "in.jsonl");
      writeFileSync(file, baselineText);
      const lines: string[] = [];
      const code = run(
        ["--input", file, "--samples", "30", "--languages", "en"],
        (l) => lines.push(l),
      );
      return { code, output: lines.join("\n") };
    });
    expect(strict.output).toContain("synthetic records are not measurements");
    expect(strict.code).toBe(1);
  });

  test("a lost first record of a session is detected", () => {
    const lines = baselineText.trimEnd().split("\n");
    lines.splice(FIRST_WARM, 1);
    const result = summarizeText(lines.join("\n") + "\n");
    expect(result.output).toContain("records 1..1 are missing");
    expect(result.code).toBe(1);
  });

  test("a lost last record of a session is detected from the end marker", () => {
    const lines = baselineText.trimEnd().split("\n");
    lines.splice(lines.length - 2, 1);
    const result = summarizeText(lines.join("\n") + "\n");
    expect(result.output).toContain("end marker says");
    expect(result.code).toBe(1);
  });

  test("a session without an end marker is reported", () => {
    const lines = baselineText.trimEnd().split("\n");
    lines.pop();
    const result = summarizeText(lines.join("\n") + "\n");
    expect(result.output).toContain("no end marker");
    expect(result.code).toBe(1);
  });

  test("language auto without a label cannot pass", () => {
    const text = mutateLine(baselineText, FIRST_WARM + 3, (r) => {
      r.language = "auto";
      delete r.label;
    });
    const result = summarizeText(text);
    expect(result.output).toContain('language "auto" and no label');
    expect(result.code).toBe(1);
  });

  test("two spoken languages are separate workloads that both must be present", () => {
    const both = generateSynthetic({ languages: ["en", "pl"] });
    const ok = summarizeText(both, ["--languages", "en,pl"]);
    expect(ok.output).toContain("pl raw 5s: n=30");
    expect(ok.code).toBe(0);
    // Dropping the pl session leaves en samples only: required pl is missing.
    const lines = both.trimEnd().split("\n");
    const onlyEn = lines.filter(
      (l) =>
        !l.includes('"label":"pl"') && !l.includes('"session":1700000001001'),
    );
    const missing = summarizeText(onlyEn.join("\n") + "\n", [
      "--languages",
      "en,pl",
    ]);
    expect(missing.output).toContain("pl raw 5s: MISSING");
    expect(missing.code).toBe(1);
    // An unexpected label is flagged instead of silently merged.
    const unexpected = summarizeText(both, ["--languages", "en"]);
    expect(unexpected.output).toContain(
      'unexpected workload language/label "pl"',
    );
    expect(unexpected.code).toBe(1);
  });

  test("protocol defaults require en and pl, 3 warmups and 5 cold starts", () => {
    const run30 = (text: string, extra: string[] = []) =>
      withTempDir((dir) => {
        const file = join(dir, "in.jsonl");
        writeFileSync(file, text);
        const lines: string[] = [];
        const code = run(
          ["--input", file, "--samples", "30", "--allow-synthetic", ...extra],
          (l) => lines.push(l),
        );
        return { code, output: lines.join("\n") };
      });
    // The spec's own command on an English-only file must not pass.
    const englishOnly = run30(baselineText);
    expect(englishOnly.output).toContain("pl raw 5s: MISSING");
    expect(englishOnly.code).toBe(1);
    // Both languages, 3 warmups and 5 cold starts present: passes by default.
    const full = run30(generateSynthetic({ languages: ["en", "pl"] }));
    expect(full.output).toContain("warmup dropped: 3");
    expect(full.output).toContain("cold starts: n=5");
    expect(full.code).toBe(0);
    // Default warmup 3: a file with only 30 samples per workload comes up short.
    const noWarmupsText = generateSynthetic({
      languages: ["en", "pl"],
      warmup: 0,
    });
    const noWarmups = run30(noWarmupsText);
    expect(noWarmups.output).toContain("need 30");
    expect(noWarmups.code).toBe(1);
    // Default cold 5: fewer cold starts fail, and --cold overrides it.
    const fewCold = generateSynthetic({ languages: ["en", "pl"], cold: 2 });
    expect(run30(fewCold).output).toContain("2 cold success(es), need 5");
    expect(run30(fewCold).code).toBe(1);
    expect(run30(fewCold, ["--cold", "2"]).code).toBe(0);
    expect(run30(noWarmupsText, ["--warmup", "0"]).code).toBe(0);
  });

  test("usage errors exit 2", () => {
    const lines: string[] = [];
    const log = (l: string) => lines.push(l);
    expect(run([], log)).toBe(2);
    expect(run(["--input", "x"], log)).toBe(2);
    expect(run(["--input", "x", "--samples", "0"], log)).toBe(2);
    expect(run(["--input", "x", "--samples", "1", "--bogus", "1"], log)).toBe(
      2,
    );
  });

  test("an unreadable input fails", () => {
    const lines: string[] = [];
    const missing = join(tmpdir(), "does-not-exist.jsonl");
    expect(
      run(["--input", missing, "--samples", "30"], (l) => lines.push(l)),
    ).toBe(1);
  });
});

describe("baseline comparison", () => {
  function compareWith(candidateText: string): {
    code: number;
    output: string;
  } {
    return withTempDir((dir) => {
      const base = join(dir, "base.jsonl");
      const cand = join(dir, "cand.jsonl");
      writeFileSync(base, baselineText);
      writeFileSync(cand, candidateText);
      const lines: string[] = [];
      const code = run(
        [
          "--input",
          cand,
          "--baseline",
          base,
          "--samples",
          "30",
          "--warmup",
          "3",
          "--allow-synthetic",
          "--languages",
          "en",
        ],
        (l) => lines.push(l),
      );
      return { code, output: lines.join("\n") };
    });
  }

  test("a required workload missing from the baseline fails the comparison", () => {
    const both = generateSynthetic({ languages: ["en", "pl"] });
    const result = withTempDir((dir) => {
      const base = join(dir, "base.jsonl");
      const cand = join(dir, "cand.jsonl");
      writeFileSync(base, baselineText); // English only
      writeFileSync(cand, both);
      const lines: string[] = [];
      const code = run(
        [
          "--input",
          cand,
          "--baseline",
          base,
          "--samples",
          "30",
          "--allow-synthetic",
        ],
        (l) => lines.push(l),
      );
      return { code, output: lines.join("\n") };
    });
    expect(result.output).toContain("pl raw 5s: no samples in baseline");
    expect(result.code).toBe(1);
  });

  test("identical data has no regression", () => {
    const { code, output } = compareWith(baselineText);
    expect(output).toContain("regression vs baseline");
    expect(output).not.toContain("REGRESSION");
    expect(code).toBe(0);
  });

  test("a 50% slower candidate regresses", () => {
    const { code, output } = compareWith(
      generateSynthetic({ scale: 1.5, cold: 5 }),
    );
    expect(output).toContain("REGRESSION");
    expect(code).toBe(1);
  });

  test("a 2% slower candidate stays inside the allowance", () => {
    const { code, output } = compareWith(
      generateSynthetic({ scale: 1.02, cold: 5 }),
    );
    expect(output).not.toContain("REGRESSION");
    expect(code).toBe(0);
  });
});

describe("routed records (HANDY-008)", () => {
  const warmCleanup = () =>
    baselineText
      .trimEnd()
      .split("\n")
      .map((line) => JSON.parse(line))
      .flatMap((r, index) =>
        r.mode === "cleanup" && !r.cold && r.outcome === "pasted"
          ? [index]
          : [],
      );

  test("routed records never supply samples, languages or the baseline", () => {
    const [first, second] = warmCleanup();
    let text = mutateLine(baselineText, first, (r) => {
      r.profile = "p1700000000000";
      r.llm_route = "default";
      r.language = "pl";
    });
    text = mutateLine(text, second, (r) => {
      r.llm_route = "english_model";
    });
    const result = summarizeText(text);
    expect(result.code).toBe(1); // Two required cleanup samples are now routed.
    expect(result.output).toContain("en cleanup 5s: n=28");
    expect(result.output).not.toContain(
      'unexpected workload language/label "pl"',
    );
    expect(result.output).toContain(
      "routed (profiles and non-default models; excluded from samples and baseline):",
    );
    expect(result.output).toContain("p1700000000000|default: n=1");
    expect(result.output).toContain("built-in|english_model: n=1");

    const parsed = parseJsonl(text);
    expect(parsed.errors).toEqual([]);
    const opts = parseArgs([
      "--input",
      "unused",
      "--samples",
      "30",
      "--warmup",
      "3",
      "--languages",
      "en",
      "--allow-synthetic",
    ]).opts;
    const summary = summarize(parsed.records, opts, parsed.ends);
    const cleanup5 = summary.workloads.find((w) => w.key === "en|cleanup|5")!;
    expect(cleanup5.available).toBe(28);
    expect(summary.routed.map((g) => g.key)).toEqual([
      "built-in|english_model",
      "p1700000000000|default",
    ]);
    expect(summary.routed[0].outcomes.pasted).toBe(1);
    expect(summary.routed[0].total?.n).toBe(1);
    expect(summary.routed[0].llmMedianUs).not.toBeNull();
  });

  test("a built-in record with the default route is not routed", () => {
    const [first] = warmCleanup();
    const result = summarizeText(
      mutateLine(baselineText, first, (r) => {
        r.llm_route = "default";
      }),
    );
    expect(result.code).toBe(0);
    expect(result.output).toContain("en cleanup 5s: n=30");
    expect(result.output).not.toContain("routed (");
  });

  test("custom command profiles appear in both assistant and routed sections", () => {
    const record = {
      ...assistantRecord(),
      profile: "p2",
      llm_route: "profile_model",
    };
    expect(validateRecord(record)).toBeNull();
    const result = summarizeText(baselineText + assistantText([record]));
    expect(result.code).toBe(0);
    expect(result.output).toContain("p2|profile_model: n=1");
    expect(result.output).toContain("assistant (separate");
  });

  test("route and profile fields are strictly validated", () => {
    const cleanup = {
      ...assistantRecord(),
      mode: "cleanup",
      outcome: "pasted",
    };
    expect(
      validateRecord({ ...cleanup, llm_route: "english_model" }),
    ).toBeNull();
    expect(
      validateRecord({ ...cleanup, mode: "raw", llm_route: "default" }),
    ).toBe("llm_route is invalid for raw mode");
    expect(
      validateRecord({ ...assistantRecord(), llm_route: "english_model" }),
    ).toBe("english_model requires cleanup mode");
    expect(validateRecord({ ...cleanup, llm_route: "teleport" })).toBe(
      "unknown llm_route",
    );
    for (const profile of ["", 5, null]) {
      expect(validateRecord({ ...cleanup, profile })).toBe(
        "profile must be a non-empty string",
      );
    }
  });
});
